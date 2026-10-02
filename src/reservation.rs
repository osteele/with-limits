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

use crate::platform::{ReservationProcess, ReservationProcessChecker};

const LOCK_FILE: &str = ".lock";
const RECORD_VERSION: u32 = 1;
const STALE_AFTER_MILLIS: u64 = 30_000;

#[derive(Clone, Debug)]
pub struct ReservationStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReservationRecord {
    #[serde(default)]
    pub version: u32,
    pub pid: u32,
    #[serde(default)]
    pub process_start_time: Option<u64>,
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

#[derive(Clone, Copy)]
enum ScanLockState {
    Held,
    NotHeld,
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
            fallback_store_path(),
        ))
    }

    pub fn inspect(
        &self,
        readings: &Readings,
        policy: &Policy,
    ) -> Result<(Verdict, OutstandingReservations)> {
        let now = unix_millis()?;
        // A read takes the lock when it can and proceeds without it when it
        // cannot. Mutual exclusion makes a read's view consistent; it is not
        // what makes the answer correct, since each record is a separate file
        // written whole. A store this process may not write is still one it
        // can usefully read, and refusing here turns a permissions problem
        // into no answer at all — which a caller reads as "unknown" and then
        // drops its memory gate over, quietly, which is the opposite of what
        // a gate is for. Reserving still requires the lock; that one is a
        // real critical section.
        let lock = self.acquire_lock_for_read();
        // Validated after the lock attempt, which creates a missing store, so
        // the directory checked is the one scanned: checking first would let
        // another account create it between the check and the scan. A store
        // validated as this account's, mode 0700, cannot then be replaced by
        // another account.
        #[cfg(unix)]
        if let Err(error) = validate_store_directory(&self.path) {
            // Records in a store another account can write prove nothing
            // about this account's work, and counting them would let that
            // account refuse admission at will.
            eprintln!("with-limits: warning: ignoring reservations: {error:#}");
            let outstanding = OutstandingReservations::default();
            let verdict = headroom::decide_with_reservations(readings, policy, outstanding);
            return Ok((verdict, outstanding));
        }
        let lock_state = if lock.is_some() {
            ScanLockState::Held
        } else {
            ScanLockState::NotHeld
        };
        let mut checker = ReservationProcessChecker::new();
        let scan = self.scan_with(now, lock_state, |pid| checker.inspect(pid))?;
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
        let mut checker = ReservationProcessChecker::new();
        let scan = self.scan_with(now, ScanLockState::Held, |pid| checker.inspect(pid))?;
        print_warnings(&scan.warnings);
        let verdict = headroom::decide_with_reservations(readings, policy, scan.outstanding);
        let reservation = if verdict.admitted {
            budget_bytes
                .map(|budget| self.publish_locked(budget, now, &mut checker))
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
        let mut checker = ReservationProcessChecker::new();
        let scan = self.scan_with(now, ScanLockState::Held, |pid| checker.inspect(pid))?;
        print_warnings(&scan.warnings);
        self.publish_locked(budget_bytes, now, &mut checker)
    }

    fn publish_locked(
        &self,
        budget_bytes: u64,
        now: u64,
        checker: &mut ReservationProcessChecker,
    ) -> Result<Reservation> {
        let pid = std::process::id();
        let process_start_time = match checker.inspect(pid)? {
            ReservationProcess::Live { start_time } => start_time,
            ReservationProcess::Dead => None,
        };
        let record = ReservationRecord {
            version: RECORD_VERSION,
            pid,
            process_start_time,
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

    /// Take the store lock for a read, or report why it could not be taken.
    ///
    /// Warned rather than swallowed: a read that could not be serialized is
    /// still answered, but an operator whose store cannot be locked should be
    /// told once.
    fn acquire_lock_for_read(&self) -> Option<StoreLock> {
        match self.acquire_lock() {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!(
                    "with-limits: reading reservations without the store lock: {error:#}; stale and invalid reservations will be left in place"
                );
                None
            }
        }
    }

    fn acquire_lock(&self) -> Result<StoreLock> {
        let file = self.open_lock_file()?;
        StoreLock::acquire(file)
            .with_context(|| format!("could not lock reservation store {}", self.path.display()))
    }

    /// Take the store lock if it is free, or return `None` when another
    /// holder has it.
    fn try_acquire_lock(&self) -> Result<Option<StoreLock>> {
        let file = self.open_lock_file()?;
        StoreLock::try_acquire(file)
            .with_context(|| format!("could not lock reservation store {}", self.path.display()))
    }

    fn open_lock_file(&self) -> Result<File> {
        ensure_store_directory(&self.path)?;
        let lock_path = self.path.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(&lock_path)
            .with_context(|| format!("could not open reservation lock {}", lock_path.display()))
    }

    fn scan_with<F>(
        &self,
        now: u64,
        lock_state: ScanLockState,
        mut inspect_process: F,
    ) -> Result<ScanResult>
    where
        F: FnMut(u32) -> Result<ReservationProcess>,
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
                remove_invalid_file(
                    &path,
                    "filename is not a process id",
                    lock_state,
                    &mut warnings,
                );
                continue;
            };
            let content = match fs::read(&path) {
                Ok(content) => content,
                Err(error) => {
                    warnings.push(format!(
                        "invalid reservation {}: could not read it: {error}; ignored and left in place",
                        path.display()
                    ));
                    continue;
                }
            };
            let record: ReservationRecord = match serde_json::from_slice(&content) {
                Ok(record) => record,
                Err(error) => {
                    handle_invalid_file(
                        &path,
                        filename_pid,
                        &format!("could not parse JSON: {error}"),
                        lock_state,
                        &mut warnings,
                        &mut inspect_process,
                    )?;
                    continue;
                }
            };
            let invalid_reason = if record.version > RECORD_VERSION {
                Some(format!(
                    "record version {} is not supported",
                    record.version
                ))
            } else if record.pid != filename_pid {
                Some(format!(
                    "record PID {} does not match filename PID {filename_pid}",
                    record.pid
                ))
            } else if record.pid == 0 || record.budget_bytes == 0 {
                Some("PID and budget must both be positive".to_owned())
            } else {
                None
            };
            if let Some(reason) = invalid_reason {
                handle_invalid_file(
                    &path,
                    filename_pid,
                    &reason,
                    lock_state,
                    &mut warnings,
                    &mut inspect_process,
                )?;
                continue;
            }

            match inspect_process(record.pid)? {
                ReservationProcess::Dead => {
                    reap_reservation(&path, "dead", lock_state, &mut warnings);
                    continue;
                }
                ReservationProcess::Live {
                    start_time: Some(actual),
                } if record
                    .process_start_time
                    .is_some_and(|expected| expected != actual) =>
                {
                    reap_reservation(&path, "reused-PID", lock_state, &mut warnings);
                    continue;
                }
                ReservationProcess::Live { .. } => {}
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

fn handle_invalid_file<F>(
    path: &Path,
    filename_pid: u32,
    reason: &str,
    lock_state: ScanLockState,
    warnings: &mut Vec<String>,
    inspect_process: &mut F,
) -> Result<()>
where
    F: FnMut(u32) -> Result<ReservationProcess>,
{
    if filename_pid != 0
        && matches!(
            inspect_process(filename_pid)?,
            ReservationProcess::Live { .. }
        )
    {
        warnings.push(format!(
            "invalid reservation {}: {reason}; ignored and left in place because PID {filename_pid} is live",
            path.display()
        ));
    } else {
        remove_invalid_file(path, reason, lock_state, warnings);
    }
    Ok(())
}

impl Reservation {
    /// Record the tree's latest RSS, or skip this tick when another process
    /// holds the store lock. The update runs on the supervisor's enforcement
    /// tick, so it must never wait for a holder that may be stalled; a record
    /// that misses ticks for 30 seconds counts its full budget, which errs on
    /// the side the reservation exists for.
    pub fn update(&mut self, observed_rss_bytes: u64) -> Result<()> {
        let now = unix_millis()?;
        let Some(_lock) = self.store.try_acquire_lock()? else {
            return Ok(());
        };
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
    fallback: PathBuf,
) -> PathBuf {
    if let Some(path) = configured.filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    xdg_runtime_dir
        .filter(|path| !path.is_empty())
        .or_else(|| tmpdir.filter(|path| !path.is_empty()))
        .map(|parent| PathBuf::from(parent).join("with-limits-reservations"))
        .unwrap_or(fallback)
}

/// The store used when neither `XDG_RUNTIME_DIR` nor `TMPDIR` names a
/// per-account directory. `/tmp` is shared by every account, so the store
/// name carries the effective user id; without it the first account to run
/// would own the store and every other account's runs would fail.
#[cfg(unix)]
fn fallback_store_path() -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/with-limits-reservations-{uid}"))
}

/// The store used when neither `XDG_RUNTIME_DIR` nor `TMPDIR` is set. The
/// Windows temporary directory is already per-account.
#[cfg(windows)]
fn fallback_store_path() -> PathBuf {
    std::env::temp_dir().join("with-limits-reservations")
}

fn unix_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system clock exceeds the reservation timestamp range")
}

fn ensure_store_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if fs::symlink_metadata(path).is_err() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not create reservation store {}", path.display())
                    })
                }
            }
        }
        validate_store_directory(path)
    }

    #[cfg(not(unix))]
    {
        if path.exists() {
            return Ok(());
        }
        fs::create_dir_all(path)
            .with_context(|| format!("could not create reservation store {}", path.display()))
    }
}

/// Refuse a store this account does not exclusively control. Another account
/// that can write the directory can plant records that block admission, or
/// symlinks that redirect this supervisor's writes into files it owns.
#[cfg(unix)]
fn validate_store_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect reservation store {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "reservation store {} is a symbolic link; refusing to use it",
            path.display()
        );
    }
    if !metadata.is_dir() {
        anyhow::bail!("reservation store {} is not a directory", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        anyhow::bail!(
            "reservation store {} is owned by uid {}, not this account (uid {uid}); refusing to use it",
            path.display(),
            metadata.uid()
        );
    }
    if metadata.mode() & 0o022 != 0 {
        anyhow::bail!(
            "reservation store {} is writable by other accounts (mode {:o}); refusing to use it",
            path.display(),
            metadata.mode() & 0o777
        );
    }
    Ok(())
}

fn write_record(path: &Path, record: &ReservationRecord) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
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

fn remove_invalid_file(
    path: &Path,
    reason: &str,
    lock_state: ScanLockState,
    warnings: &mut Vec<String>,
) {
    let mut warning = format!("invalid reservation {}: {reason}; ignored", path.display());
    match lock_state {
        ScanLockState::Held => match fs::remove_file(path) {
            Ok(()) => warning.push_str(" and removed"),
            Err(error) => warning.push_str(&format!("; could not remove it: {error}")),
        },
        ScanLockState::NotHeld => {
            warning.push_str(" and left in place because the reservation store is not locked");
        }
    }
    warnings.push(warning);
}

fn reap_reservation(
    path: &Path,
    reason: &str,
    lock_state: ScanLockState,
    warnings: &mut Vec<String>,
) {
    if matches!(lock_state, ScanLockState::NotHeld) {
        return;
    }
    if let Err(error) = fs::remove_file(path) {
        warnings.push(format!(
            "could not reap {reason} reservation {}: {error}; ignored and left in place",
            path.display()
        ));
    }
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
        lock_file(&file, true)?;
        Ok(Self { file })
    }

    fn try_acquire(file: File) -> io::Result<Option<Self>> {
        Ok(lock_file(&file, false)?.then_some(Self { file }))
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if let Err(error) = unlock_file(&self.file) {
            eprintln!("with-limits: warning: could not unlock reservation store: {error}");
        }
    }
}

/// Lock `file` exclusively. A blocking call waits and returns `true`; a
/// non-blocking call returns `false` when another holder has the lock.
#[cfg(unix)]
fn lock_file(file: &File, blocking: bool) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let operation = if blocking {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if !blocking && error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
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

/// Lock `file` exclusively. A blocking call waits and returns `true`; a
/// non-blocking call returns `false` when another holder has the lock.
#[cfg(windows)]
fn lock_file(file: &File, blocking: bool) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::ERROR_LOCK_VIOLATION,
        Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY},
        System::IO::OVERLAPPED,
    };

    let flags = if blocking {
        LOCKFILE_EXCLUSIVE_LOCK
    } else {
        LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
    };
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if !blocking && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(error)
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    .expect("make test reservation directory private");
            }
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

    fn fixture_record(
        pid: u32,
        process_start_time: Option<u64>,
        budget_bytes: u64,
        observed_rss_bytes: u64,
        observed_at_unix_millis: u64,
    ) -> ReservationRecord {
        ReservationRecord {
            version: RECORD_VERSION,
            pid,
            process_start_time,
            budget_bytes,
            observed_rss_bytes,
            observed_at_unix_millis,
        }
    }

    fn healthy_readings() -> Readings {
        Readings {
            pressure_level: Some(1),
            swap_free_bytes: Some(4 << 30),
            swap_total_bytes: Some(8 << 30),
            load_per_cpu: Some(0.5),
            available_bytes: Some(12 << 30),
            total_bytes: Some(16 << 30),
            available_fraction: Some(0.75),
            memory_free_percent: Some(75.0),
        }
    }

    fn exited_process_id() -> u32 {
        let executable = std::env::current_exe().expect("locate the reservation test executable");
        let mut child = std::process::Command::new(executable)
            .args(["--exact", "with_limits_test_that_does_not_exist"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start a process for the dead-reservation fixture");
        let pid = child.id();
        let status = child
            .wait()
            .expect("wait for the dead-reservation fixture process");
        assert!(status.success());
        pid
    }

    #[test]
    fn a_read_answers_without_the_store_lock() {
        // A store this process cannot lock is still one it can read: refusing
        // here would hand a consumer "unknown", which it treats as a dropped
        // memory gate rather than as a refusal.
        let directory = TestDirectory::new("readonly-lock");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(exited_process_id(), None, 4 << 30, 1 << 30, 100);
        write_fixture(&store, &record);
        fs::create_dir(directory.0.join(LOCK_FILE)).expect("make the reservation lock unavailable");

        let (verdict, outstanding) = store
            .inspect(&healthy_readings(), &Policy::default())
            .expect("answer admission despite the unavailable lock and dead record");

        assert!(verdict.admitted);
        assert_eq!(outstanding.count, 0);
        assert_eq!(outstanding.unrealized_bytes, 0);
        assert!(store.record_path(record.pid).exists());
    }

    #[test]
    fn resolves_the_configured_and_default_store_paths() {
        assert_eq!(
            resolve_store_path(
                Some("/configured".into()),
                Some("/xdg".into()),
                Some("/tmpdir".into()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/configured")
        );
        assert_eq!(
            resolve_store_path(
                None,
                Some("/xdg".into()),
                Some("/tmpdir".into()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/xdg/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(
                None,
                None,
                Some("/tmpdir".into()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/tmpdir/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(None, None, None, PathBuf::from("/fallback")),
            PathBuf::from("/fallback")
        );
        assert_eq!(
            resolve_store_path(
                Some(OsString::new()),
                Some("/xdg".into()),
                Some("/tmpdir".into()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/xdg/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(
                None,
                Some(OsString::new()),
                Some("/tmpdir".into()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/tmpdir/with-limits-reservations")
        );
        assert_eq!(
            resolve_store_path(
                None,
                None,
                Some(OsString::new()),
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/fallback")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_shared_tmp_fallback_is_per_account() {
        let uid = unsafe { libc::geteuid() };
        assert_eq!(
            fallback_store_path(),
            PathBuf::from(format!("/tmp/with-limits-reservations-{uid}"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_store_other_accounts_could_write_or_redirect() {
        use std::os::unix::fs::PermissionsExt;

        let parent = TestDirectory::new("untrusted-store");
        for (name, mode) in [("group", 0o770), ("world", 0o707)] {
            let shared = parent.0.join(name);
            fs::create_dir(&shared).expect("create shared store");
            fs::set_permissions(&shared, fs::Permissions::from_mode(mode))
                .expect("make the store writable by others");
            let error = ReservationStore::new(shared.clone())
                .reserve(1)
                .err()
                .unwrap_or_else(|| panic!("a {name}-writable store is refused"));
            assert!(format!("{error:#}").contains("writable by other accounts"));
            assert!(!shared.join(std::process::id().to_string()).exists());
        }

        let private = parent.0.join("private");
        fs::create_dir(&private).expect("create private store");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
            .expect("make the store private");
        let link = parent.0.join("link");
        std::os::unix::fs::symlink(&private, &link).expect("link to the private store");
        let error = ReservationStore::new(link)
            .reserve(1)
            .err()
            .expect("a symlinked store is refused");
        assert!(format!("{error:#}").contains("symbolic link"));
    }

    #[cfg(unix)]
    #[test]
    fn a_read_ignores_records_in_a_store_other_accounts_can_write() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("untrusted-read");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(
            std::process::id(),
            None,
            15 << 30,
            0,
            unix_millis().unwrap(),
        );
        write_fixture(&store, &record);
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o777))
            .expect("make the store world-writable");

        let (verdict, outstanding) = store
            .inspect(&healthy_readings(), &Policy::default())
            .expect("answer admission over an untrusted store");
        assert!(
            verdict.admitted,
            "a planted reservation must not refuse admission"
        );
        assert_eq!(outstanding.count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_record_write_does_not_follow_a_planted_symlink() {
        let directory = TestDirectory::new("planted-record");
        let victim = directory.0.join("victim");
        fs::write(&victim, "original").expect("write victim file");
        let store = ReservationStore::new(directory.0.join("store"));
        store.acquire_lock().expect("create the store");
        std::os::unix::fs::symlink(&victim, store.record_path(std::process::id()))
            .expect("plant a symlink at the record path");

        assert!(store.reserve(1).is_err());
        assert_eq!(
            fs::read_to_string(&victim).expect("read victim file"),
            "original"
        );
    }

    #[test]
    fn sums_fresh_unrealized_bytes_and_uses_full_stale_budgets() {
        let directory = TestDirectory::new("arithmetic");
        let store = ReservationStore::new(directory.0.clone());
        let now = 100_000;
        for record in [
            fixture_record(101, Some(1_001), 1_000, 400, now - STALE_AFTER_MILLIS),
            fixture_record(102, Some(1_002), 900, 800, now - STALE_AFTER_MILLIS - 1),
            fixture_record(103, Some(1_003), 500, 700, now),
        ] {
            write_fixture(&store, &record);
        }

        let scan = store
            .scan_with(now, ScanLockState::Held, |pid| {
                Ok(ReservationProcess::Live {
                    start_time: Some(u64::from(pid) + 900),
                })
            })
            .expect("scan reservation fixtures");
        assert_eq!(scan.outstanding.count, 3);
        assert_eq!(scan.outstanding.unrealized_bytes, 1_500);
        assert!(scan.warnings.is_empty());
    }

    #[test]
    fn a_locked_read_reaps_a_dead_reservation() {
        let directory = TestDirectory::new("read-reap");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(
            exited_process_id(),
            None,
            4 << 30,
            0,
            unix_millis().unwrap(),
        );
        write_fixture(&store, &record);

        let (_, outstanding) = store
            .inspect(&healthy_readings(), &Policy::default())
            .expect("read reservations");
        assert_eq!(outstanding.count, 0);
        assert!(!store.record_path(record.pid).exists());
    }

    #[test]
    fn a_zero_budget_record_is_invalid() {
        let directory = TestDirectory::new("zero-budget");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(301, None, 0, 0, 100);
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live { start_time: None })
            })
            .expect("scan zero-budget record");
        assert_eq!(scan.outstanding.count, 0);
        assert!(scan.warnings[0].contains("must both be positive"));
    }

    #[cfg(unix)]
    #[test]
    fn releasing_reports_a_record_it_could_not_remove() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("release-denied");
        let store = ReservationStore::new(directory.0.clone());
        let mut reservation = store.reserve(1).expect("publish reservation");
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o500))
            .expect("make the store read-only");
        let result = reservation.release();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))
            .expect("restore the store");
        assert!(result.is_err(), "a failed removal must be reported");
        reservation
            .release()
            .expect("release once the store is writable");
    }

    #[test]
    fn releasing_an_already_removed_record_succeeds() {
        let directory = TestDirectory::new("release-missing");
        let store = ReservationStore::new(directory.0.clone());
        let mut reservation = store.reserve(1).expect("publish reservation");
        fs::remove_file(store.record_path(std::process::id())).expect("remove record");
        reservation.release().expect("release a missing record");
    }

    #[test]
    fn reaps_a_dead_process_reservation() {
        let directory = TestDirectory::new("dead");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(201, Some(2_001), 1_000, 0, 100);
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| Ok(ReservationProcess::Dead))
            .expect("scan dead reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert!(!store.record_path(record.pid).exists());
    }

    #[test]
    fn a_dead_reap_failure_does_not_fail_a_locked_scan() {
        let directory = TestDirectory::new("dead-reap-failure");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(202, Some(2_002), 1_000, 0, 100);
        let path = store.record_path(record.pid);
        write_fixture(&store, &record);
        let reap_target = path.clone();

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                fs::remove_file(&reap_target).expect("remove reservation before reap");
                fs::create_dir(&reap_target).expect("replace reservation with a directory");
                Ok(ReservationProcess::Dead)
            })
            .expect("answer despite the failed dead-reservation reap");

        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("could not reap dead reservation"));
        assert!(scan.warnings[0].contains("ignored and left in place"));
        assert!(path.is_dir());
    }

    #[test]
    fn keeps_and_reports_an_unparsable_live_reservation() {
        let directory = TestDirectory::new("unparsable");
        let store = ReservationStore::new(directory.0.clone());
        let path = store.record_path(301);
        fs::write(&path, b"{broken").expect("write unparsable reservation fixture");

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live { start_time: None })
            })
            .expect("scan unparsable reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("could not parse JSON"));
        assert!(scan.warnings[0].contains("left in place because PID 301 is live"));
        assert!(path.exists());
    }

    #[test]
    fn removes_and_reports_an_unparsable_dead_reservation() {
        let directory = TestDirectory::new("unparsable-dead");
        let store = ReservationStore::new(directory.0.clone());
        let path = store.record_path(302);
        fs::write(&path, b"{broken").expect("write unparsable reservation fixture");

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| Ok(ReservationProcess::Dead))
            .expect("scan unparsable reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("ignored and removed"));
        assert!(!path.exists());
    }

    #[test]
    fn an_unlocked_scan_leaves_an_invalid_reservation_in_place() {
        let directory = TestDirectory::new("unlocked-invalid");
        let store = ReservationStore::new(directory.0.clone());
        let path = directory.0.join("not-a-pid");
        fs::write(&path, b"not a reservation").expect("write invalid reservation fixture");

        let scan = store
            .scan_with(100, ScanLockState::NotHeld, |_| {
                panic!("a non-PID filename must not trigger process inspection")
            })
            .expect("scan invalid reservation without the store lock");

        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(
            scan.warnings[0].contains("left in place because the reservation store is not locked")
        );
        assert!(path.exists());
    }

    #[test]
    fn accepts_unknown_record_fields() {
        let directory = TestDirectory::new("unknown-field");
        let store = ReservationStore::new(directory.0.clone());
        let path = store.record_path(303);
        let content = serde_json::json!({
            "version": RECORD_VERSION,
            "pid": 303,
            "process_start_time": 3_003,
            "budget_bytes": 1_000,
            "observed_rss_bytes": 250,
            "observed_at_unix_millis": 100,
            "future_field": "accepted"
        });
        fs::write(
            &path,
            serde_json::to_vec(&content).expect("serialize reservation fixture"),
        )
        .expect("write reservation fixture");

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live {
                    start_time: Some(3_003),
                })
            })
            .expect("scan reservation fixture with an unknown field");
        assert_eq!(scan.outstanding.count, 1);
        assert_eq!(scan.outstanding.unrealized_bytes, 750);
        assert!(scan.warnings.is_empty());
        assert!(path.exists());
    }

    #[test]
    fn keeps_an_unsupported_live_record_version() {
        let directory = TestDirectory::new("future-version");
        let store = ReservationStore::new(directory.0.clone());
        let mut record = fixture_record(304, Some(3_004), 1_000, 0, 100);
        record.version = RECORD_VERSION + 1;
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live {
                    start_time: Some(3_004),
                })
            })
            .expect("scan unsupported live reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("record version 2 is not supported"));
        assert!(store.record_path(record.pid).exists());
    }

    #[test]
    fn reaps_a_reservation_after_pid_reuse() {
        let directory = TestDirectory::new("reused-pid");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(305, Some(3_005), 1_000, 0, 100);
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live {
                    start_time: Some(9_999),
                })
            })
            .expect("scan reused-PID reservation fixture");
        assert_eq!(scan.outstanding.count, 0);
        assert!(!store.record_path(record.pid).exists());
    }

    #[test]
    fn an_unlocked_scan_does_not_reap_a_reused_pid() {
        let directory = TestDirectory::new("unlocked-reused-pid");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(306, Some(3_006), 1_000, 0, 100);
        let path = store.record_path(record.pid);
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::NotHeld, |_| {
                Ok(ReservationProcess::Live {
                    start_time: Some(9_999),
                })
            })
            .expect("scan reused-PID reservation without the store lock");

        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert!(path.exists());
    }

    #[test]
    fn a_reused_pid_reap_failure_does_not_fail_a_locked_scan() {
        let directory = TestDirectory::new("reused-pid-reap-failure");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(307, Some(3_007), 1_000, 0, 100);
        let path = store.record_path(record.pid);
        write_fixture(&store, &record);
        let reap_target = path.clone();

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                fs::remove_file(&reap_target).expect("remove reservation before reap");
                fs::create_dir(&reap_target).expect("replace reservation with a directory");
                Ok(ReservationProcess::Live {
                    start_time: Some(9_999),
                })
            })
            .expect("answer despite the failed reused-PID reservation reap");

        assert_eq!(scan.outstanding.count, 0);
        assert_eq!(scan.outstanding.unrealized_bytes, 0);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("could not reap reused-PID reservation"));
        assert!(scan.warnings[0].contains("ignored and left in place"));
        assert!(path.is_dir());
    }

    #[test]
    fn falls_back_to_pid_liveness_when_start_time_is_unavailable() {
        let directory = TestDirectory::new("unknown-start-time");
        let store = ReservationStore::new(directory.0.clone());
        let record = fixture_record(308, Some(3_008), 1_000, 250, 100);
        write_fixture(&store, &record);

        let scan = store
            .scan_with(100, ScanLockState::Held, |_| {
                Ok(ReservationProcess::Live { start_time: None })
            })
            .expect("scan reservation without a readable process start time");
        assert_eq!(scan.outstanding.count, 1);
        assert_eq!(scan.outstanding.unrealized_bytes, 750);
        assert!(store.record_path(record.pid).exists());
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
        assert_eq!(record.version, RECORD_VERSION);
        assert_eq!(record.pid, std::process::id());
        let expected_start_time = match ReservationProcessChecker::new()
            .inspect(std::process::id())
            .expect("inspect current process")
        {
            ReservationProcess::Live { start_time } => start_time,
            ReservationProcess::Dead => panic!("current process is not live"),
        };
        assert_eq!(record.process_start_time, expected_start_time);
        assert_eq!(record.budget_bytes, 4_096);
        assert_eq!(record.observed_rss_bytes, 1_024);
        let wall_clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as u64;
        assert!(wall_clock.abs_diff(record.observed_at_unix_millis) < 60_000);

        drop(reservation);
        assert!(!path.exists());
    }

    #[test]
    fn an_update_skips_a_tick_while_another_holder_has_the_lock() {
        // The update runs on the supervisor's enforcement tick. A holder that
        // stalls — stopped by job control, or slow to scan a large store —
        // must cost this supervisor one skipped refresh, never its limits.
        let directory = TestDirectory::new("contended-update");
        let store = ReservationStore::new(directory.0.clone());
        let mut reservation = store.reserve(4_096).expect("publish reservation");
        let path = store.record_path(std::process::id());
        let held = store.acquire_lock().expect("hold the store lock");

        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = reservation
                .update(1_024)
                .map_err(|error| format!("{error:#}"));
            sender.send(result).expect("report update result");
            reservation
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("an update must not wait for a contended store lock");
        assert_eq!(result, Ok(()));
        let mut reservation = worker.join().expect("join update thread");
        let record: ReservationRecord =
            serde_json::from_slice(&fs::read(&path).expect("read reservation record"))
                .expect("parse reservation record");
        assert_eq!(
            record.observed_rss_bytes, 0,
            "a skipped tick writes nothing"
        );

        drop(held);
        reservation
            .update(2_048)
            .expect("update after the lock is free");
        let record: ReservationRecord =
            serde_json::from_slice(&fs::read(&path).expect("read reservation record"))
                .expect("parse reservation record");
        assert_eq!(record.observed_rss_bytes, 2_048);
        drop(reservation);
        assert!(!path.exists());
    }

    #[test]
    fn the_process_checker_distinguishes_live_and_exited_processes() {
        let mut checker = ReservationProcessChecker::new();
        match checker
            .inspect(std::process::id())
            .expect("inspect current process")
        {
            // A start time is what lets a scan tell a reused PID from the
            // original; losing it degrades identity to liveness alone.
            ReservationProcess::Live { start_time } => {
                assert!(start_time.is_some(), "no start time for a live process")
            }
            ReservationProcess::Dead => panic!("current process reported dead"),
        }
        assert!(matches!(
            checker
                .inspect(exited_process_id())
                .expect("inspect exited process"),
            ReservationProcess::Dead
        ));
    }

    #[cfg(unix)]
    #[test]
    fn creates_the_store_and_files_with_private_permissions() {
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
        assert_eq!(
            fs::metadata(path.join(LOCK_FILE))
                .expect("inspect reservation lock")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let reservation = store.reserve(1).expect("publish reservation");
        assert_eq!(
            fs::metadata(store.record_path(std::process::id()))
                .expect("inspect reservation record")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(reservation);
    }
}
