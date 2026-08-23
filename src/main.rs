mod platform;
mod process_tree;

use anyhow::{bail, Context, Result};
use clap::Parser;
use process_tree::ProcessTree;
use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};
use sysinfo::System;
use with_limits::{format_bytes, CpuLimit, HumanDuration, MemorySpec};

const EXIT_TIMEOUT: u8 = 124;
const EXIT_WRAPPER_ERROR: u8 = 125;
const EXIT_CANNOT_INVOKE: u8 = 126;
const EXIT_NOT_FOUND: u8 = 127;
const EXIT_MEMORY: u8 = 137;
const DEFAULT_NICE_ADJUSTMENT: i32 = 10;
const NICE_ENV: &str = "WITH_LIMITS_NICE";

#[cfg(windows)]
const WINDOWS_HELPER_ARG: &str = "--internal-windows-launch-helper";
#[cfg(windows)]
const WINDOWS_GATE_ENV: &str = "WITH_LIMITS_WINDOWS_GATE";

#[derive(Debug, Parser)]
#[command(version, about, trailing_var_arg = true)]
struct Cli {
    /// Maximum resident memory for the command and its descendants
    #[arg(short = 'm', long, value_name = "SIZE")]
    memory: Option<MemorySpec>,

    /// Sustained CPU allowance, measured in logical cores
    #[arg(long, value_name = "CORES")]
    cpu: Option<CpuLimit>,

    /// Wall-clock time limit
    #[arg(short = 't', long, value_name = "DURATION")]
    time: Option<HumanDuration>,

    /// Wait this long after graceful termination before forcing termination
    #[arg(
        long,
        visible_alias = "grace",
        default_value = "2s",
        value_name = "DURATION"
    )]
    kill_after: HumanDuration,

    /// Run a command string through the platform shell
    #[arg(
        short = 'c',
        long = "shell-command",
        value_name = "COMMAND",
        conflicts_with = "command"
    )]
    shell_command: Option<String>,

    /// Shell executable used by -c
    #[arg(long, value_name = "PATH", requires = "shell_command")]
    shell: Option<OsString>,

    /// Fail instead of using sampled enforcement
    #[arg(long)]
    require_native: bool,

    /// Suppress the startup summary (limit violations are always reported)
    #[arg(short, long)]
    quiet: bool,

    #[arg(long, hide = true, default_value = "250ms")]
    poll_interval: HumanDuration,

    /// Command and arguments to run (place them after --)
    #[arg(value_name = "COMMAND", allow_hyphen_values = true)]
    command: Vec<OsString>,
}

enum CommandResult {
    Status(ExitStatus),
    Code(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MemoryBudget {
    process_limit: Option<u64>,
    host_reserve: Option<u64>,
}

fn main() {
    #[cfg(windows)]
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(WINDOWS_HELPER_ARG)) {
        windows_launch_helper();
    }

    let code = match run() {
        Ok(CommandResult::Status(status)) => status_exit_code(status),
        Ok(CommandResult::Code(code)) => i32::from(code),
        Err(error) => {
            eprintln!("with-limits: {error:#}");
            i32::from(EXIT_WRAPPER_ERROR)
        }
    };
    std::process::exit(code);
}

fn run() -> Result<CommandResult> {
    let cli = Cli::parse();
    if cli.shell_command.is_none() && cli.command.is_empty() {
        bail!("specify a command after --, or use -c");
    }

    let mut system = System::new_all();
    system.refresh_memory();
    let memory_spec = if cli.memory.is_none() && cli.cpu.is_none() && cli.time.is_none() {
        Some(MemorySpec::AvailableFraction(0.7))
    } else {
        cli.memory
    };
    let available = if matches!(memory_spec, Some(MemorySpec::AvailableFraction(_))) {
        available_memory(&mut system)?
    } else {
        system.available_memory()
    };
    let memory_budget = resolve_memory_budget(memory_spec, available)?;
    let memory = memory_budget.process_limit;
    let host_memory_reserve = memory_budget.host_reserve;
    let cpu = cli.cpu.map(|limit| limit.0);
    let nice_adjustment = configured_nice_adjustment()?;
    let mut command = build_command(&cli)?;
    command.env("WITH_LIMITS_ACTIVE", "1");

    let mut controller = platform::Controller::prepare(&mut command, memory, cpu, nice_adjustment)?;
    if cli.require_native {
        if memory.is_some() && !controller.memory_is_native() {
            bail!("native memory enforcement is unavailable on this platform");
        }
        if cpu.is_some() && !controller.cpu_is_native() {
            bail!("native CPU enforcement is unavailable on this platform");
        }
    }
    if !cli.quiet && io::stderr().is_terminal() {
        print_summary(&cli, memory, nice_adjustment, &controller);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!(
                "with-limits: command not found: {}",
                command.get_program().to_string_lossy()
            );
            return Ok(CommandResult::Code(EXIT_NOT_FOUND));
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            eprintln!(
                "with-limits: cannot invoke {}: {error}",
                command.get_program().to_string_lossy()
            );
            return Ok(CommandResult::Code(EXIT_CANNOT_INVOKE));
        }
        Err(error) => return Err(error).context("could not start command"),
    };
    if let Err(error) = controller.attach(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("could not place command under resource control");
    }

    supervise(
        &cli,
        memory,
        host_memory_reserve,
        cpu,
        child,
        controller,
        &mut system,
    )
}

fn resolve_memory_budget(spec: Option<MemorySpec>, available: u64) -> Result<MemoryBudget> {
    let process_limit = spec
        .map(|limit| limit.resolve(available))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let host_reserve = match (spec, process_limit) {
        (Some(MemorySpec::AvailableFraction(_)), Some(limit)) => {
            Some(available.saturating_sub(limit))
        }
        _ => None,
    };
    Ok(MemoryBudget {
        process_limit,
        host_reserve,
    })
}

fn host_reserve_crossed(reserve: u64, available: u64) -> bool {
    reserve > 0 && available < reserve
}

fn configured_nice_adjustment() -> Result<Option<i32>> {
    parse_nice_adjustment(std::env::var_os(NICE_ENV).as_deref())
}

fn parse_nice_adjustment(value: Option<&OsStr>) -> Result<Option<i32>> {
    let Some(value) = value else {
        return Ok(Some(DEFAULT_NICE_ADJUSTMENT));
    };
    let value = value
        .to_str()
        .context("WITH_LIMITS_NICE contains non-UTF-8 text")?
        .trim();
    if ["0", "off", "false", "no"]
        .iter()
        .any(|disabled| value.eq_ignore_ascii_case(disabled))
    {
        return Ok(None);
    }
    let adjustment = value
        .parse::<i32>()
        .context("WITH_LIMITS_NICE must be an integer from 1 through 19, or off")?;
    if !(1..=19).contains(&adjustment) {
        bail!("WITH_LIMITS_NICE must be from 1 through 19, or off");
    }
    Ok(Some(adjustment))
}

fn available_memory(system: &mut System) -> Result<u64> {
    system.refresh_memory();
    let available = system.available_memory();
    if available > 0 {
        return Ok(available);
    }

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/memory_pressure")
            .arg("-Q")
            .output()
            .context("could not run memory_pressure to determine available memory")?;
        if !output.status.success() {
            bail!(
                "memory_pressure could not determine available memory: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let output = String::from_utf8(output.stdout)
            .context("memory_pressure returned non-UTF-8 output")?;
        parse_memory_pressure_available(&output)
    }

    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .context("could not read /proc/meminfo to determine available memory")?;
        parse_linux_mem_available(&meminfo)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("the platform reported no available memory");
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_mem_available(meminfo: &str) -> Result<u64> {
    let fields: Vec<_> = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .context("/proc/meminfo does not contain MemAvailable")?
        .split_whitespace()
        .collect();
    let [value, unit] = fields.as_slice() else {
        bail!("MemAvailable has an unexpected field layout");
    };
    if *unit != "kB" {
        bail!("MemAvailable uses unexpected unit {unit:?}");
    }
    let kibibytes = value
        .parse::<u64>()
        .context("MemAvailable is not an unsigned integer")?;
    if kibibytes == 0 {
        bail!("MemAvailable reports zero available memory");
    }
    kibibytes
        .checked_mul(1024)
        .context("MemAvailable overflows a byte count")
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_pressure_available(output: &str) -> Result<u64> {
    let total = output
        .lines()
        .find_map(|line| line.strip_prefix("The system has "))
        .and_then(|rest| rest.split_whitespace().next())
        .context("memory_pressure output does not report total memory")?
        .parse::<u64>()
        .context("memory_pressure total memory is not an unsigned integer")?;
    let percent = output
        .lines()
        .find_map(|line| line.strip_prefix("System-wide memory free percentage:"))
        .map(str::trim)
        .and_then(|value| value.strip_suffix('%'))
        .context("memory_pressure output does not report a percentage")?
        .parse::<f64>()
        .context("memory_pressure percentage is not numeric")?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        bail!("memory_pressure returned an invalid free-memory percentage");
    }
    let available = (total as f64 * percent / 100.0) as u64;
    if available == 0 {
        bail!("memory_pressure reports zero available memory");
    }
    Ok(available)
}

fn build_command(cli: &Cli) -> Result<Command> {
    let target = build_target_command(cli)?;
    #[cfg(windows)]
    return wrap_windows_command(target);
    #[cfg(not(windows))]
    Ok(target)
}

fn build_target_command(cli: &Cli) -> Result<Command> {
    if let Some(script) = &cli.shell_command {
        #[cfg(unix)]
        {
            let shell = cli.shell.clone().unwrap_or_else(default_unix_shell);
            let mut command = Command::new(shell);
            command.arg("-c").arg(script);
            return Ok(command);
        }
        #[cfg(windows)]
        {
            let shell = cli
                .shell
                .clone()
                .or_else(|| std::env::var_os("COMSPEC"))
                .unwrap_or_else(|| "cmd.exe".into());
            let mut command = Command::new(shell);
            command.args(["/D", "/S", "/C"]).arg(script);
            return Ok(command);
        }
    }
    let (program, arguments) = cli.command.split_first().context("missing command")?;
    let mut command = Command::new(program);
    command.args(arguments);
    Ok(command)
}

#[cfg(windows)]
fn wrap_windows_command(target: Command) -> Result<Command> {
    let mut command = Command::new(
        std::env::current_exe().context("could not locate the with-limits executable")?,
    );
    command
        .arg(WINDOWS_HELPER_ARG)
        .arg(target.get_program())
        .args(target.get_args());
    Ok(command)
}

#[cfg(windows)]
fn windows_launch_helper() -> ! {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{
            ExitProcess, OpenEventW, WaitForSingleObject, EVENT_ALL_ACCESS, INFINITE,
        },
    };

    let fail = |message: &str, code: u32| -> ! {
        eprintln!("with-limits: {message}");
        unsafe { ExitProcess(code) }
    };
    let gate_name = std::env::var(WINDOWS_GATE_ENV)
        .unwrap_or_else(|_| fail("Windows launch helper did not receive its gate", 125));
    let gate_name: Vec<u16> = gate_name.encode_utf16().chain(Some(0)).collect();
    let gate = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, gate_name.as_ptr()) };
    if gate.is_null() {
        fail("Windows launch helper could not open its gate", 125);
    }
    let wait = unsafe { WaitForSingleObject(gate, INFINITE) };
    unsafe { CloseHandle(gate) };
    if wait != WAIT_OBJECT_0 {
        fail("Windows launch helper could not wait for its gate", 125);
    }

    let mut args = std::env::args_os().skip(2);
    let program = args
        .next()
        .unwrap_or_else(|| fail("Windows launch helper did not receive a command", 125));
    let mut command = Command::new(&program);
    command.args(args).env_remove(WINDOWS_GATE_ENV);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!(
                "with-limits: command not found: {}",
                display_program(&program)
            );
            unsafe { ExitProcess(u32::from(EXIT_NOT_FOUND)) }
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            eprintln!(
                "with-limits: cannot invoke {}: {error}",
                display_program(&program)
            );
            unsafe { ExitProcess(u32::from(EXIT_CANNOT_INVOKE)) }
        }
        Err(error) => fail(&format!("could not start command: {error}"), 125),
    };
    let status = child
        .wait()
        .unwrap_or_else(|error| fail(&format!("could not wait for command: {error}"), 125));
    let code = status.code().unwrap_or(i32::from(EXIT_WRAPPER_ERROR)) as u32;
    unsafe { ExitProcess(code) }
}

#[cfg(unix)]
fn default_unix_shell() -> OsString {
    if cfg!(target_os = "macos") {
        OsString::from("/bin/zsh")
    } else {
        OsString::from("/bin/sh")
    }
}

fn print_summary(
    cli: &Cli,
    memory: Option<u64>,
    nice_adjustment: Option<i32>,
    controller: &platform::Controller,
) {
    let mut limits = Vec::new();
    if let Some(bytes) = memory {
        let method = if controller.memory_is_native() {
            "native"
        } else {
            "sampled"
        };
        limits.push(format!("memory {} ({method})", format_bytes(bytes)));
    }
    if let Some(CpuLimit(cores)) = cli.cpu {
        let method = if controller.cpu_is_native() {
            "native"
        } else {
            "sampled"
        };
        limits.push(format!("CPU {cores} cores ({method})"));
    }
    if let Some(limit) = cli.time {
        limits.push(format!("time {limit}"));
    }
    if let Some(adjustment) = nice_adjustment {
        if cfg!(windows) {
            limits.push("priority below normal".into());
        } else {
            limits.push(format!("niceness +{adjustment}"));
        }
    }
    eprintln!("with-limits: {}", limits.join(", "));
}

fn supervise(
    cli: &Cli,
    memory: Option<u64>,
    host_memory_reserve: Option<u64>,
    cpu: Option<f64>,
    mut child: Child,
    mut controller: platform::Controller,
    system: &mut System,
) -> Result<CommandResult> {
    let started = Instant::now();
    let deadline = cli
        .time
        .map(|limit| {
            started
                .checked_add(limit.0)
                .context("time limit is too large for the platform clock")
        })
        .transpose()?;
    let mut tree = ProcessTree::new(child.id(), !cfg!(windows));
    let mut child_status = None;
    let mut throttle = CpuThrottle::new();
    let mut last_cpu_sample = started;
    let mut cpu_sample_initialized = false;
    let mut next_host_memory_check = started;

    loop {
        let mut root_exited = false;
        if child_status.is_none() {
            child_status = child
                .try_wait()
                .context("could not inspect command status")?;
            if child_status.is_some() {
                root_exited = true;
            }
        }
        let usage = tree.refresh(system);
        if root_exited {
            tree.retire_root();
        }
        let targets = tree.identities();
        controller.set_targets(&targets)?;

        if let Some(status) = child_status {
            if cfg!(windows) || tree.is_empty() {
                controller.disarm();
                return Ok(CommandResult::Status(status));
            }
        } else if !tree.observed_root() {
            child_status = child
                .try_wait()
                .context("could not recheck an unobserved command")?;
            if let Some(status) = child_status {
                controller.disarm();
                return Ok(CommandResult::Status(status));
            }
            bail!("could not observe the running command in the process table");
        }

        if let Some(limit) = memory {
            if usage.rss_bytes > limit {
                eprintln!(
                    "with-limits: memory limit exceeded ({} > {})",
                    format_bytes(usage.rss_bytes),
                    format_bytes(limit)
                );
                stop_command(&mut child, &controller, &mut tree, system, cli.kill_after.0)?;
                controller.disarm();
                return Ok(CommandResult::Code(EXIT_MEMORY));
            }
        }

        if let Some(reserve) = host_memory_reserve {
            if reserve > 0 && Instant::now() >= next_host_memory_check {
                let available = available_memory(system)?;
                next_host_memory_check = Instant::now()
                    .checked_add(Duration::from_secs(1))
                    .context("platform clock cannot schedule memory sampling")?;
                if host_reserve_crossed(reserve, available) {
                    eprintln!(
                        "with-limits: host memory reserve crossed ({} available < {} reserved)",
                        format_bytes(available),
                        format_bytes(reserve)
                    );
                    stop_command(&mut child, &controller, &mut tree, system, cli.kill_after.0)?;
                    controller.disarm();
                    return Ok(CommandResult::Code(EXIT_MEMORY));
                }
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            eprintln!("with-limits: time limit exceeded");
            stop_command(&mut child, &controller, &mut tree, system, cli.kill_after.0)?;
            controller.disarm();
            return Ok(CommandResult::Code(EXIT_TIMEOUT));
        }

        if let Some(cores) = cpu {
            if !controller.cpu_is_native() {
                if !cpu_sample_initialized {
                    if let Err(error) = controller.suspend(&targets) {
                        let _ = controller.resume(&targets);
                        return Err(error);
                    }
                    sleep_until_or_deadline(
                        sysinfo::MINIMUM_CPU_UPDATE_INTERVAL + Duration::from_millis(10),
                        deadline,
                    );
                    tree.refresh(system);
                    let targets = tree.identities();
                    controller.set_targets(&targets)?;
                    controller.resume(&targets)?;
                    last_cpu_sample = Instant::now();
                    cpu_sample_initialized = true;
                    continue;
                }
                let now = Instant::now();
                throttle.record(usage.cpu_percent, cores, now - last_cpu_sample)?;
                last_cpu_sample = now;
                throttle.pause(&controller, &targets, deadline)?;
                if !throttle.pause_debt.is_zero() {
                    continue;
                }
            }
        }
        thread::sleep(cli.poll_interval.0);
    }
}

struct CpuThrottle {
    pause_debt: Duration,
}

impl CpuThrottle {
    fn new() -> Self {
        Self {
            pause_debt: Duration::ZERO,
        }
    }

    fn record(&mut self, observed_percent: f64, cores: f64, sample: Duration) -> Result<()> {
        let allowed_percent = cores * 100.0;
        if observed_percent <= allowed_percent {
            return Ok(());
        }
        let added = Duration::try_from_secs_f64(
            sample.as_secs_f64() * (observed_percent / allowed_percent - 1.0),
        )
        .context("CPU throttle interval is too large")?;
        self.pause_debt = self
            .pause_debt
            .checked_add(added)
            .context("CPU throttle debt overflowed")?;
        Ok(())
    }

    fn pause(
        &mut self,
        controller: &platform::Controller,
        targets: &[process_tree::ProcessIdentity],
        deadline: Option<Instant>,
    ) -> Result<()> {
        let pause = self.pending_pause(Duration::from_secs(1));
        if pause.is_zero() {
            return Ok(());
        }

        if let Err(error) = controller.suspend(targets) {
            let _ = controller.resume(targets);
            return Err(error);
        }
        let result = sleep_until_or_deadline(pause, deadline);
        let resume_result = controller.resume(targets);
        self.consume_pause(result);
        resume_result
    }

    fn pending_pause(&self, maximum: Duration) -> Duration {
        self.pause_debt.min(maximum)
    }

    fn consume_pause(&mut self, duration: Duration) {
        self.pause_debt = self.pause_debt.saturating_sub(duration);
    }
}

fn sleep_until_or_deadline(duration: Duration, deadline: Option<Instant>) -> Duration {
    let started = Instant::now();
    while started.elapsed() < duration {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(25)));
    }
    started.elapsed().min(duration)
}

fn stop_command(
    child: &mut Child,
    controller: &platform::Controller,
    tree: &mut ProcessTree,
    system: &mut System,
    grace: Duration,
) -> Result<()> {
    tree.refresh(system);
    let mut targets = tree.identities();
    controller.set_targets(&targets)?;
    controller.terminate(&targets)?;
    let deadline = Instant::now()
        .checked_add(grace)
        .context("termination grace period is too large")?;
    while Instant::now() < deadline {
        child
            .try_wait()
            .context("could not inspect command during termination")?;
        tree.refresh(system);
        targets = tree.identities();
        controller.set_targets(&targets)?;
        if tree.is_empty() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25).min(grace));
    }
    targets = tree.identities();
    controller.kill(&targets)?;
    let verification_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < verification_deadline {
        child
            .try_wait()
            .context("could not inspect command after forced termination")?;
        tree.refresh(system);
        targets = tree.identities();
        controller.set_targets(&targets)?;
        if tree.is_empty() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "process tree still contains {} process(es) after forced termination",
        targets.len()
    )
}

#[cfg(unix)]
fn status_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        i32::from(EXIT_WRAPPER_ERROR)
    }
}

#[cfg(windows)]
fn status_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(i32::from(EXIT_WRAPPER_ERROR))
}

#[allow(dead_code)]
fn display_program(program: &OsStr) -> String {
    program.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACOS_MEMORY_PRESSURE: &str = "The system has 17179869184 (1048576 pages with a page size of 16384).\nSystem-wide memory free percentage: 34%\n";

    #[test]
    fn parses_linux_mem_available_fixture() {
        let fixture = "MemTotal:       16384000 kB\nMemFree:         1024000 kB\nMemAvailable:    8388608 kB\nBuffers:          128000 kB\n";
        assert_eq!(
            parse_linux_mem_available(fixture).unwrap(),
            8 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn rejects_linux_meminfo_schema_drift_and_invalid_values() {
        for fixture in [
            "MemTotal: 1000 kB\n",
            "MemAvailable: 12 MB\n",
            "MemAvailable: many kB\n",
            "MemAvailable: 12 kB extra\n",
            "MemAvailable: 0 kB\n",
            "MemAvailable: 18446744073709551615 kB\n",
        ] {
            assert!(
                parse_linux_mem_available(fixture).is_err(),
                "unexpectedly accepted {fixture:?}"
            );
        }
    }

    #[test]
    fn parses_macos_memory_pressure_fixture() {
        assert_eq!(
            parse_memory_pressure_available(MACOS_MEMORY_PRESSURE).unwrap(),
            5_841_155_522
        );
    }

    #[test]
    fn rejects_macos_memory_pressure_schema_drift_and_invalid_values() {
        for fixture in [
            "System-wide memory free percentage: 34%\n",
            "The system has 17179869184 bytes.\n",
            "The system has many bytes.\nSystem-wide memory free percentage: 34%\n",
            "The system has 17179869184 bytes.\nSystem-wide memory free percentage: many%\n",
            "The system has 17179869184 bytes.\nSystem-wide memory free percentage: 101%\n",
            "The system has 17179869184 bytes.\nSystem-wide memory free percentage: 0%\n",
        ] {
            assert!(
                parse_memory_pressure_available(fixture).is_err(),
                "unexpectedly accepted {fixture:?}"
            );
        }
    }

    #[test]
    fn cpu_throttle_accumulates_and_consumes_multi_interval_debt() {
        let mut throttle = CpuThrottle::new();
        throttle.record(400.0, 1.0, Duration::from_secs(1)).unwrap();

        assert_eq!(
            throttle.pending_pause(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        throttle.consume_pause(Duration::from_secs(1));
        assert_eq!(
            throttle.pending_pause(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        throttle.consume_pause(Duration::from_secs(1));
        throttle.consume_pause(Duration::from_secs(1));
        assert_eq!(throttle.pause_debt, Duration::ZERO);
    }

    #[test]
    fn cpu_throttle_ignores_samples_within_the_allowance() {
        let mut throttle = CpuThrottle::new();
        throttle.record(49.9, 0.5, Duration::from_secs(10)).unwrap();
        assert_eq!(throttle.pause_debt, Duration::ZERO);
    }

    #[test]
    fn cpu_throttle_rejects_non_finite_samples() {
        let mut throttle = CpuThrottle::new();
        assert!(throttle
            .record(f64::NAN, 1.0, Duration::from_secs(1))
            .is_err());
        assert!(throttle
            .record(f64::INFINITY, 1.0, Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn percentage_memory_budget_preserves_a_shared_host_reserve() {
        let budget =
            resolve_memory_budget(Some(MemorySpec::AvailableFraction(0.7)), 10_000).unwrap();
        assert_eq!(
            budget,
            MemoryBudget {
                process_limit: Some(7_000),
                host_reserve: Some(3_000),
            }
        );

        assert!(!host_reserve_crossed(3_000, 3_000));
        assert!(host_reserve_crossed(3_000, 2_999));
        assert!(
            host_reserve_crossed(budget.host_reserve.unwrap(), 2_999),
            "every agent launched from the same snapshot observes the same crossed reserve"
        );
    }

    #[test]
    fn absolute_memory_budget_does_not_claim_a_host_reserve() {
        assert_eq!(
            resolve_memory_budget(Some(MemorySpec::Bytes(4_096)), 10_000).unwrap(),
            MemoryBudget {
                process_limit: Some(4_096),
                host_reserve: None,
            }
        );
        assert_eq!(
            resolve_memory_budget(None, 10_000).unwrap(),
            MemoryBudget {
                process_limit: None,
                host_reserve: None,
            }
        );
    }

    #[test]
    fn parses_nice_adjustment_and_disable_values() {
        assert_eq!(parse_nice_adjustment(None).unwrap(), Some(10));
        assert_eq!(
            parse_nice_adjustment(Some(OsStr::new(" 5 "))).unwrap(),
            Some(5)
        );
        for value in ["0", "off", "OFF", "false", "no"] {
            assert_eq!(
                parse_nice_adjustment(Some(OsStr::new(value))).unwrap(),
                None
            );
        }
    }

    #[test]
    fn rejects_invalid_nice_adjustments() {
        for value in ["", "-1", "20", "low", "1.5"] {
            assert!(
                parse_nice_adjustment(Some(OsStr::new(value))).is_err(),
                "unexpectedly accepted {value:?}"
            );
        }
    }
}
