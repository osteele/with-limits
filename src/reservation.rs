use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use with_limits::headroom::{self, OutstandingReservations, Policy, Readings, Verdict};

const LOCK_FILE: &str = ".lock";
const STALE_AFTER_MILLIS: u64 = 30_000;

#[derive(Clone, Debug)]
pub struct ReservationStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReservationRecord {
    pub pid: u32,
    pub budget_bytes: u64,
    pub observed_rss_bytes: u64,
    pub observed_at_unix_millis: u64,
}

pub struct Admission {
    pub verdict: Verdict,
    pub reservation: Option<Reservation>,
}

pub struct Reservation {
    store: ReservationStore,
    record: ReservationRecord,
    active: bool,
}

struct ScanResult {
    outstanding: OutstandingReservations,
    warnings: Vec<String>,
}

impl ReservationStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn configured() -> Self {
        Self::new(resolve_store_path(
            std::env::var_os("WITH_LIMITS_RESERVATION_DIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            std::env::var_os("TMPDIR"),
        ))
    }

    pub fn inspect(
        &self,
        readings: &Readings,
        policy: &Policy,
    ) -> Result<(Verdict, OutstandingReservations)> {
        let now = unix_millis()?;
        let _lock = self.acquire_lock()?;
        let scan = self.scan_with(now, crate::platform::process_is_alive)?;
        print_warnings(&scan.warnings);
        let verdict = headroom::decide_with_reservations(readings, policy, scan.outstanding);
        Ok((verdict, scan.outstanding))
    }

    pub fn decide_and_reserve(
        &self,
        readings: &Readings,
        policy: &Policy,
        budget_bytes: Option<u64>,
    ) -> Result<Admission> {
        let now = unix_millis()?;
        let _lock = self.acquire_lock()?;
        let scan = self.scan_with(now, crate::platform::process_is_alive)?;
        print_warnings(&scan.warnings);
        let verdict = headroom::decide_with_reservations(readings, policy, scan.outstanding);
        let reservation = if verdict.admitted {
            budget_bytes
                .map(|budget| self.publish_locked(budget, now))
                .transpose()?
        } else {
            None
        };
        Ok(Admission {
            verdict,
            reservation,
        })
    }

    pub fn reserve(&self, budget_bytes: u64) -> Result<Reservation> {
        let now = unix_millis()?;
        let _lock = self.acquire_lock()?;
        let scan = self.scan_with(now, crate::platform::process_is_alive)?;
        print_warnings(&scan.warnings);
        self.publish_locked(budget_bytes, now)
    }

    fn publish_locked(&self, budget_bytes: u64, now: u64) -> Result<Reservation> {
        let record = ReservationRecord {
            pid: std::process::id(),
            budget_bytes,
            observed_rss_bytes: 0,
            observed_at_unix_millis: now,
        };
        write_record(&self.record_path(record.pid), &record)?;
        Ok(Reservation {
            store: self.clone(),
            record,
            active: true,
        })
    }

    fn acquire_lock(&self) -> Result<StoreLock> {
        ensure_store_directory(&self.path)?;
        let lock_path = self.path.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("could not open reservation lock {}", lock_path.display()))?;
        StoreLock::acquire(file)
            .with_context(|| format!("could not lock reservation store {}", self.path.display()))
    }

    fn scan_with<F>(&self, now: u64, mut process_is_alive: F) -> Result<ScanResult>
    where
        F: FnMut(u32) -> Result<Option<bool>>,
    {
        let mut outstanding = OutstandingReservations::default();
        let mut warnings = Vec::new();
        for entry in fs::read_dir(&self.path)
            .with_context(|| format!("could not read reservation store {}", self.path.display()))?
        {
            let entry = entry.with_context(|| {
                format!(
                    "could not read an entry in reservation store {}",
                    self.path.display()
                )
            })?;
            if entry.file_name() == LOCK_FILE {
                continue;
            }
            let path = entry.path();
            if !entry
                .file_type()
                .with_context(|| format!("could not inspect {}", path.display()))?
                .is_file()
            {
                continue;
            }
            let filename_pid = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok());
            let Some(filename_pid) = filename_pid else {
                remove_invalid_file(&path, "filename is not a process id", &mut warnings);
                continue;
            };
            let content = match fs::read(&path) {
                Ok(content) => content,
                Err(error) => {
                    remove_invalid_file(
                        &path,
                        &format!("could not read it: {error}"),
                        &mut warnings,
                    );
                    continue;
                }
            };
            let record: ReservationRecord = match serde_json::from_slice(&content) {
                Ok(record) => record,
                Err(error) => {
                    remove_invalid_file(
                        &path,
                        &format!("could not parse JSON: {error}"),
                        &mut warnings,
                    );
                    continue;
                }
            };
            if record.pid != filename_pid {
                remove_invalid_file(
                    &path,
                    &format!(
                        "record PID {} does not match filename PID {filename_pid}",
                        record.pid
                    ),
                    &mut warnings,
                );
                continue;
            }
            if record.pid == 0 || record.budget_bytes == 0 {
                remove_invalid_file(&path, "PID and budget must both be positive", &mut warnings);
                continue;
            }
            if process_is_alive(record.pid)? == Some(false) {
                fs::remove_file(&path).with_context(|| {
                    format!("could not reap dead reservation {}", path.display())
                })?;
                continue;
            }

            let age = now.saturating_sub(record.observed_at_unix_millis);
            let unrealized = if age > STALE_AFTER_MILLIS {
                record.budget_bytes
            } else {
                record
                    .budget_bytes
                    .saturating_sub(record.observed_rss_bytes)
            };
            outstanding.count = outstanding.count.saturating_add(1);
            outstanding.unrealized_bytes = outstanding.unrealized_bytes.saturating_add(unrealized);
        }
        Ok(ScanResult {
            outstanding,
            warnings,
        })
    }

    fn record_path(&self, pid: u32) -> PathBuf {
        self.path.join(pid.to_string())
    }
}

impl Reservation {
    pub fn update(&mut self, observed_rss_bytes: u64) -> Result<()> {
        let now = unix_millis()?;
        let _lock = self.store.acquire_lock()?;
        self.record.observed_rss_bytes = observed_rss_bytes;
        self.record.observed_at_unix_millis = now;
        write_record(&self.store.record_path(self.record.pid), &self.record)
    }

    fn release(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let _lock = self.store.acquire_lock()?;
        let path = self.store.record_path(self.record.pid);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not remove reservation {}", path.display()));
            }
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("with-limits: could not release memory reservation: {error:#}");
        }
    }
}

pub fn resolve_store_path(
    configured: Option<OsString>,
    xdg_runtime_dir: Option<OsString>,
    tmpdir: Option<OsString>,
) -> PathBuf {
    if let Some(path) = configured {
        return PathBuf::from(path);
    }
    let parent = xdg_runtime_dir
        .or(tmpdir)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    parent.join("with-limits-reservations")
}

fn unix_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system clock exceeds the reservation timestamp range")
}

fn ensure_store_directory(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        match builder.create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("could not create reservation store {}", path.display())),
        }
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .with_context(|| format!("could not create reservation store {}", path.display()))
    }
}

fn write_record(path: &Path, record: &ReservationRecord) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("could not open reservation {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("could not serialize reservation {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("could not finish reservation {}", path.display()))?;
    file.flush()
        .with_context(|| format!("could not flush reservation {}", path.display()))
}

fn remove_invalid_file(path: &Path, reason: &str, warnings: &mut Vec<String>) {
    let mut warning = format!("invalid reservation {}: {reason}; ignored", path.display());
    match fs::remove_file(path) {
        Ok(()) => warning.push_str(" and removed"),
        Err(error) => warning.push_str(&format!("; could not remove it: {error}")),
    }
    warnings.push(warning);
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("with-limits: warning: {warning}");
    }
}

struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire(file: File) -> io::Result<Self> {
        lock_file(&file)?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if let Err(error) = unlock_file(&self.file) {
            eprintln!("with-limits: warning: could not unlock reservation store: {error}");
        }
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn lock_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK},
        System::IO::OVERLAPPED,
    };

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &mut overlapped,
        )
    } != 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    if unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "with-limits-reservation-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("remove stale test reservation directory");
            }
            fs::create_dir(&path).expect("create test reservation directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_fixture(store: &ReservationStore, record: &ReservationRecord) {
        write_record(&store.record_path(record.pid), record).expect("write reservation fixture");
    }

    #[test]
    fn resolves_the_configured_and_default_store_paths() {
        assert_eq!(
            resolve_store_path(
                Some("/configured".into()),
                Some("/xdg".into()),
                Some("/tmpdir".into())
            ),
            PathBuf::from("/configured")
        );
        assert_eq!(
            resolve_store_path(None, Some("/xdg".into()), Some("/tmpdir".into())),
            PathBuf::from("/xdg/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(None, None, Some("/tmpdir".into())),
            PathBuf::from("/tmpdir/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(None, None, None),
            PathBuf::from("/tmp/with-limits-reservations")
        );
    }

    #[test]
    fn sums_fresh_unrealized_bytes_and_uses_full_stale_budgets() {
        let directory = TestDirectory::new("arithmetic");
        let store = ReservationStore::new(directory.0.clone());
        let now = 100_000;
        for record in [
            ReservationRecord {
                pid: 101,
                budget_bytes: 1_000,
                observed_rss_bytes: 400,
                observed_at_unix_millis: now - STALE_AFTER_MILLIS,
            },
            ReservationRecord {
                pid: 102,
                budget_bytes: 900,
                observed_rss_bytes: 800,
                observed_at_unix_millis: now - STALE_AFTER_MILLIS - 1,
            },
            ReservationRecord {
                pid: 103,
                budget_bytes: 500,
                observed_rss_bytes: 700,
                observed_at_unix_millis: now,
            },
        ] {
            write_fixture(&store, &record);
        }

        let scan = store
            .scan_with(now, |_| Ok(Some(true)))
            .expect("scan reservation fixtures");
        assert_eq!(scan.outstanding.count, 3);
        assert_eq!(scan.outstanding.unrealized_bytes, 1_500);
        assert!(scan.warnings.is_empty());
    }

    #[test]
    fn reaps_a_dead_process_reservation() {
        let directory = TestDirectory::new("dead");
        let store = ReservationStore::new(directory.0.clone());
        let record = ReservationRecord {
            pid: 201,
            budget_bytes: 1_000,
            observed_rss_bytes: 0,
            observed_at_unix_millis: 100,
        };
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, |pid| Ok(Some(pid != record.pid)))
            .expect("scan dead reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert!(!store.record_path(record.pid).exists());
    }

    #[test]
    fn removes_and_reports_unparsable_reservations() {
        let directory = TestDirectory::new("unparsable");
        let store = ReservationStore::new(directory.0.clone());
        let path = store.record_path(301);
        fs::write(&path, b"{broken").expect("write unparsable reservation fixture");

        let scan = store
            .scan_with(100, |_| Ok(Some(true)))
            .expect("scan unparsable reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("could not parse JSON"));
        assert!(scan.warnings[0].contains("ignored and removed"));
        assert!(!path.exists());
    }

    #[test]
    fn updates_the_record_and_releases_it_on_drop() {
        let directory = TestDirectory::new("lifecycle");
        let store = ReservationStore::new(directory.0.clone());
        let mut reservation = store.reserve(4_096).expect("publish reservation");
        let path = store.record_path(std::process::id());
        reservation.update(1_024).expect("update reservation");

        let content = fs::read_to_string(&path).expect("read reservation record");
        let record: ReservationRecord =
            serde_json::from_str(&content).expect("parse reservation record");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.budget_bytes, 4_096);
        assert_eq!(record.observed_rss_bytes, 1_024);
        assert!(record.observed_at_unix_millis > 0);

        drop(reservation);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn creates_the_store_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let parent = TestDirectory::new("permissions");
        let path = parent.0.join("store");
        let store = ReservationStore::new(path.clone());
        let lock = store.acquire_lock().expect("create and lock store");
        assert_eq!(
            fs::metadata(&path)
                .expect("inspect store")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(lock);
    }
}
