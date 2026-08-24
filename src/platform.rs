use crate::process_tree::ProcessIdentity;
use anyhow::Result;
use std::process::{Child, Command};

#[cfg(unix)]
mod imp {
    use super::*;
    use anyhow::{bail, Context};
    use signal_hook::consts::signal::{
        SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGUSR1, SIGUSR2, SIGWINCH,
    };
    use signal_hook::iterator::Signals;
    use std::{
        io,
        os::unix::process::CommandExt,
        sync::{
            mpsc::{self, Receiver, RecvTimeoutError, Sender},
            Arc, RwLock,
        },
        thread,
        time::Duration,
    };

    pub struct Controller {
        pgid: i32,
        targets: Arc<RwLock<Vec<ProcessIdentity>>>,
        signals: Option<Signals>,
        signal_sender: Option<Sender<()>>,
        forwarded_signals: Receiver<()>,
        armed: bool,
    }

    impl Controller {
        pub fn prepare(
            command: &mut Command,
            memory: Option<u64>,
            cpu: Option<f64>,
            nice_adjustment: Option<i32>,
        ) -> Result<Self> {
            let _ = (memory, cpu);
            command.process_group(0);
            if let Some(adjustment) = nice_adjustment {
                unsafe {
                    command.pre_exec(move || lower_priority(adjustment));
                }
            }
            let signals = Signals::new([
                SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH, SIGTSTP, SIGCONT,
            ])?;
            let (signal_sender, forwarded_signals) = mpsc::channel();
            Ok(Self {
                pgid: 0,
                targets: Arc::new(RwLock::new(Vec::new())),
                signals: Some(signals),
                signal_sender: Some(signal_sender),
                forwarded_signals,
                armed: true,
            })
        }

        pub fn attach(&mut self, child: &Child) -> Result<()> {
            self.pgid = i32::try_from(child.id()).context("child PID is too large")?;
            if self.pgid <= 1 {
                bail!("refusing unsafe child process group {}", self.pgid);
            }
            let pgid = self.pgid;
            let supervisor_pid =
                i32::try_from(std::process::id()).context("supervisor PID is too large")?;
            let targets = Arc::clone(&self.targets);
            let mut signals = self
                .signals
                .take()
                .context("signal forwarding is already attached")?;
            let signal_sender = self
                .signal_sender
                .take()
                .context("signal notification is already attached")?;
            thread::spawn(move || {
                for signal in signals.forever() {
                    let current = targets
                        .read()
                        .map(|value| value.clone())
                        .unwrap_or_default();
                    if let Err(error) = signal_processes(pgid, &current, signal) {
                        eprintln!("with-limits: could not forward signal {signal}: {error:#}");
                    }
                    if matches!(signal, SIGHUP | SIGINT | SIGQUIT | SIGTERM) {
                        let _ = signal_sender.send(());
                    } else if signal == SIGTSTP {
                        // The installed handler suppresses SIGTSTP's default action.
                        // Stop only after the command tree has received the signal.
                        if let Err(error) = signal_one(supervisor_pid, libc::SIGSTOP) {
                            eprintln!("with-limits: could not stop after SIGTSTP: {error:#}");
                        }
                    }
                }
            });
            Ok(())
        }

        pub fn wait_for_forwarded_signal(&self, timeout: Duration) -> bool {
            match self.forwarded_signals.recv_timeout(timeout) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => false,
            }
        }

        pub fn set_targets(&self, targets: &[ProcessIdentity]) -> Result<()> {
            *self
                .targets
                .write()
                .map_err(|_| anyhow::anyhow!("process-target state is poisoned"))? =
                targets.to_vec();
            Ok(())
        }

        pub fn terminate(&self, targets: &[ProcessIdentity]) -> Result<()> {
            signal_processes(self.pgid, targets, SIGTERM)
        }

        pub fn kill(&self, targets: &[ProcessIdentity]) -> Result<()> {
            signal_processes(self.pgid, targets, libc::SIGKILL)
        }

        pub fn suspend(&self, targets: &[ProcessIdentity]) -> Result<()> {
            signal_processes(self.pgid, targets, libc::SIGSTOP)
        }

        pub fn resume(&self, targets: &[ProcessIdentity]) -> Result<()> {
            signal_processes(self.pgid, targets, libc::SIGCONT)
        }

        pub fn cpu_is_native(&self) -> bool {
            false
        }

        pub fn memory_is_native(&self) -> bool {
            false
        }

        pub fn disarm(&mut self) {
            self.armed = false;
        }
    }

    fn lower_priority(adjustment: i32) -> io::Result<()> {
        let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let target = current.saturating_add(adjustment).min(19);
        if target <= current {
            return Ok(());
        }
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, target) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let targets = self
                .targets
                .read()
                .map(|value| value.clone())
                .unwrap_or_default();
            let _ = signal_processes(self.pgid, &targets, libc::SIGKILL);
        }
    }

    fn signal_processes(pgid: i32, targets: &[ProcessIdentity], signal: i32) -> Result<()> {
        if pgid <= 1 {
            bail!("refusing unsafe child process group {pgid}");
        }

        let group_error = signal_one(-pgid, signal).err();
        let group_failed = group_error.is_some();
        let mut first_error = None;
        let mut signaled_individually = false;
        for target in targets {
            let pid = i32::try_from(target.pid).context("tracked PID is too large")?;
            if pid <= 1 {
                continue;
            }
            let escaped = group_failed
                || match process_group(pid) {
                    Ok(group) => group != Some(pgid),
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        true
                    }
                };
            if escaped {
                if let Err(error) = signal_one(pid, signal) {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                } else {
                    signaled_individually = true;
                }
            }
        }
        if first_error.is_none() && group_failed && !signaled_individually {
            first_error = group_error;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn process_group(pid: i32) -> Result<Option<i32>> {
        let group = unsafe { libc::getpgid(pid) };
        if group >= 0 {
            return Ok(Some(group));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        Err(error).with_context(|| format!("could not inspect process target {pid}"))
    }

    fn signal_one(pid: i32, signal: i32) -> Result<()> {
        if unsafe { libc::kill(pid, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error).with_context(|| format!("could not signal process target {pid}"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{bail, Context};
    use std::{
        mem::size_of,
        os::windows::io::AsRawHandle,
        ptr::null,
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
            },
            Threading::{
                CreateEventW, GetActiveProcessorCount, SetEvent, BELOW_NORMAL_PRIORITY_CLASS,
            },
        },
    };

    const ALL_PROCESSOR_GROUPS: u16 = 0xffff;
    const GATE_ENV: &str = "WITH_LIMITS_WINDOWS_GATE";

    pub struct Controller {
        job: HANDLE,
        gate: HANDLE,
    }

    impl Controller {
        pub fn prepare(
            command: &mut Command,
            memory: Option<u64>,
            cpu: Option<f64>,
            nice_adjustment: Option<i32>,
        ) -> Result<Self> {
            let _ = memory;
            let gate_name = format!(
                "Local\\with-limits-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock is before the Unix epoch")?
                    .as_nanos()
            );
            let job = unsafe { CreateJobObjectW(null(), null()) };
            if job.is_null() {
                bail!(
                    "could not create a Windows Job Object: {}",
                    std::io::Error::last_os_error()
                );
            }
            let gate_name_wide: Vec<u16> = gate_name.encode_utf16().chain(Some(0)).collect();
            let gate = unsafe { CreateEventW(null(), 1, 0, gate_name_wide.as_ptr()) };
            if gate.is_null() {
                unsafe { CloseHandle(job) };
                bail!(
                    "could not create the Windows launch gate: {}",
                    std::io::Error::last_os_error()
                );
            }
            command.env(GATE_ENV, gate_name);

            let controller = Self { job, gate };
            controller.configure(cpu, nice_adjustment.is_some())?;
            Ok(controller)
        }

        fn configure(&self, cpu: Option<f64>, lower_priority: bool) -> Result<()> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if lower_priority {
                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PRIORITY_CLASS;
                limits.BasicLimitInformation.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS;
            }
            set_job_info(
                self.job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )?;

            if let Some(cores) = cpu {
                let processors = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1);
                let raw_rate = (cores / f64::from(processors)) * 10_000.0;
                if raw_rate < 1.0 {
                    bail!("CPU limit is below the Windows Job Object resolution on this host");
                }
                let rate = raw_rate.floor().min(10_000.0) as u32;
                let mut cpu_limits: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION =
                    unsafe { std::mem::zeroed() };
                cpu_limits.ControlFlags =
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                cpu_limits.Anonymous.CpuRate = rate;
                set_job_info(
                    self.job,
                    JobObjectCpuRateControlInformation,
                    &cpu_limits as *const _ as *const _,
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>(),
                )?;
            }
            Ok(())
        }

        pub fn attach(&mut self, child: &Child) -> Result<()> {
            let process = child.as_raw_handle() as HANDLE;
            if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
                bail!(
                    "could not assign launch helper to Windows Job Object: {}",
                    std::io::Error::last_os_error()
                );
            }
            if unsafe { SetEvent(self.gate) } == 0 {
                bail!(
                    "could not release the Windows launch gate: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }

        pub fn set_targets(&self, targets: &[ProcessIdentity]) -> Result<()> {
            let _ = targets;
            Ok(())
        }

        pub fn terminate(&self, targets: &[ProcessIdentity]) -> Result<()> {
            let _ = targets;
            self.terminate_job(1)
        }

        pub fn kill(&self, targets: &[ProcessIdentity]) -> Result<()> {
            let _ = targets;
            self.terminate_job(137)
        }

        pub fn suspend(&self, targets: &[ProcessIdentity]) -> Result<()> {
            let _ = targets;
            Ok(())
        }

        pub fn resume(&self, targets: &[ProcessIdentity]) -> Result<()> {
            let _ = targets;
            Ok(())
        }

        fn terminate_job(&self, code: u32) -> Result<()> {
            if unsafe { TerminateJobObject(self.job, code) } == 0 {
                bail!(
                    "could not terminate Windows Job Object: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }

        pub fn cpu_is_native(&self) -> bool {
            true
        }

        pub fn memory_is_native(&self) -> bool {
            false
        }

        pub fn disarm(&mut self) {}
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.gate);
                CloseHandle(self.job);
            }
        }
    }

    fn set_job_info(
        job: HANDLE,
        class: i32,
        data: *const core::ffi::c_void,
        size: usize,
    ) -> Result<()> {
        let size = u32::try_from(size).context("Windows Job Object setting is too large")?;
        if unsafe { SetInformationJobObject(job, class, data, size) } == 0 {
            bail!(
                "could not configure Windows Job Object: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

pub use imp::Controller;
