use anyhow::Result;
use std::process::{Child, Command};

#[cfg(unix)]
mod imp {
    use super::*;
    use anyhow::{bail, Context};
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    use std::os::unix::process::CommandExt;
    use std::thread;

    pub struct Controller {
        pgid: i32,
    }

    impl Controller {
        pub fn prepare(
            command: &mut Command,
            memory: Option<u64>,
            cpu: Option<f64>,
        ) -> Result<Self> {
            let _ = (memory, cpu);
            command.process_group(0);
            Ok(Self { pgid: 0 })
        }

        pub fn attach(&mut self, child: &Child) -> Result<()> {
            self.pgid = i32::try_from(child.id()).context("child PID is too large")?;
            if self.pgid <= 1 {
                bail!("refusing unsafe child process group {}", self.pgid);
            }
            let pgid = self.pgid;
            let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
            thread::spawn(move || {
                for signal in signals.forever() {
                    // The child owns this newly created process group.
                    unsafe { libc::kill(-pgid, signal) };
                }
            });
            Ok(())
        }

        pub fn terminate(&self) {
            self.signal(SIGTERM);
        }

        pub fn kill(&self) {
            self.signal(libc::SIGKILL);
        }

        pub fn suspend(&self) {
            self.signal(libc::SIGSTOP);
        }

        pub fn resume(&self) {
            self.signal(libc::SIGCONT);
        }

        fn signal(&self, signal: i32) {
            if self.pgid > 1 {
                // A negative PID addresses the process group, not unrelated processes.
                unsafe { libc::kill(-self.pgid, signal) };
            }
        }

        pub fn cpu_is_native(&self) -> bool {
            false
        }

        pub fn memory_is_native(&self) -> bool {
            false
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{bail, Context};
    use std::{mem::size_of, os::windows::io::AsRawHandle, ptr::null};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::GetActiveProcessorCount,
        },
    };

    const ALL_PROCESSOR_GROUPS: u16 = 0xffff;

    pub struct Controller {
        job: HANDLE,
    }

    impl Controller {
        pub fn prepare(
            command: &mut Command,
            memory: Option<u64>,
            cpu: Option<f64>,
        ) -> Result<Self> {
            let _ = command;
            let job = unsafe { CreateJobObjectW(null(), null()) };
            if job.is_null() {
                bail!(
                    "could not create a Windows Job Object: {}",
                    std::io::Error::last_os_error()
                );
            }
            let controller = Self { job };
            controller.configure(memory, cpu)?;
            Ok(controller)
        }

        fn configure(&self, memory: Option<u64>, cpu: Option<f64>) -> Result<()> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(bytes) = memory {
                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                limits.JobMemoryLimit =
                    usize::try_from(bytes).context("memory limit exceeds platform size")?;
            }
            set_job_info(
                self.job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )?;

            if let Some(cores) = cpu {
                let processors = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1);
                let rate = ((cores / f64::from(processors)) * 10_000.0)
                    .round()
                    .clamp(1.0, 10_000.0) as u32;
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
                    "could not assign child to Windows Job Object: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }

        pub fn terminate(&self) {
            unsafe { TerminateJobObject(self.job, 1) };
        }

        pub fn kill(&self) {
            unsafe { TerminateJobObject(self.job, 137) };
        }

        pub fn suspend(&self) {}
        pub fn resume(&self) {}
        pub fn cpu_is_native(&self) -> bool {
            true
        }
        pub fn memory_is_native(&self) -> bool {
            true
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.job) };
        }
    }

    fn set_job_info(
        job: HANDLE,
        class: i32,
        data: *const core::ffi::c_void,
        size: usize,
    ) -> Result<()> {
        if unsafe { SetInformationJobObject(job, class, data, u32::try_from(size).unwrap()) } == 0 {
            bail!(
                "could not configure Windows Job Object: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

pub use imp::Controller;
