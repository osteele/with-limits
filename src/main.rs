mod platform;
mod process_tree;

use anyhow::{bail, Context, Result};
use clap::Parser;
use process_tree::ProcessTree;
use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal},
    process::{Child, Command, ExitCode, ExitStatus},
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

fn main() -> ExitCode {
    match run() {
        Ok(CommandResult::Status(status)) => status_exit_code(status),
        Ok(CommandResult::Code(code)) => ExitCode::from(code),
        Err(error) => {
            eprintln!("with-limits: {error:#}");
            ExitCode::from(EXIT_WRAPPER_ERROR)
        }
    }
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
        available_memory(&system)?
    } else {
        system.available_memory()
    };
    let memory = memory_spec.map(|limit| limit.resolve(available));
    let cpu = cli.cpu.map(|limit| limit.0);
    let mut command = build_command(&cli)?;
    command.env("WITH_LIMITS_ACTIVE", "1");

    let mut controller = platform::Controller::prepare(&mut command, memory, cpu)?;
    if cli.require_native {
        if memory.is_some() && !controller.memory_is_native() {
            bail!("native memory enforcement is unavailable on this platform");
        }
        if cpu.is_some() && !controller.cpu_is_native() {
            bail!("native CPU enforcement is unavailable on this platform");
        }
    }
    if !cli.quiet && io::stderr().is_terminal() {
        print_summary(&cli, memory, &controller);
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

    supervise(&cli, memory, cpu, child, controller, &mut system)
}

fn available_memory(system: &System) -> Result<u64> {
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
        let total = output
            .lines()
            .find_map(|line| line.split_once("system has ").map(|(_, rest)| rest))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok());
        let percent = output
            .lines()
            .find_map(|line| {
                line.split_once("memory free percentage:")
                    .map(|(_, rest)| rest.trim())
            })
            .and_then(|value| value.trim_end_matches('%').parse::<f64>().ok());
        if let (Some(total), Some(percent)) = (total, percent) {
            let available = (total as f64 * percent / 100.0) as u64;
            if available > 0 {
                return Ok(available);
            }
        }
        bail!("could not parse available memory from memory_pressure output");
    }

    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .context("could not read /proc/meminfo to determine available memory")?;
        if let Some(kibibytes) = meminfo.lines().find_map(|line| {
            line.strip_prefix("MemAvailable:")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        }) {
            if kibibytes > 0 {
                return Ok(kibibytes.saturating_mul(1024));
            }
        }
        bail!("could not parse MemAvailable from /proc/meminfo");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("the platform reported no available memory");
}

fn build_command(cli: &Cli) -> Result<Command> {
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

#[cfg(unix)]
fn default_unix_shell() -> OsString {
    if cfg!(target_os = "macos") {
        OsString::from("/bin/zsh")
    } else {
        OsString::from("/bin/sh")
    }
}

fn print_summary(cli: &Cli, memory: Option<u64>, controller: &platform::Controller) {
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
    eprintln!("with-limits: {}", limits.join(", "));
}

fn supervise(
    cli: &Cli,
    memory: Option<u64>,
    cpu: Option<f64>,
    mut child: Child,
    controller: platform::Controller,
    system: &mut System,
) -> Result<CommandResult> {
    let started = Instant::now();
    let deadline = cli.time.map(|limit| started + limit.0);
    let mut tree = ProcessTree::new(child.id());
    let mut child_status = None;

    loop {
        let usage = tree.refresh(system);
        if child_status.is_none() {
            child_status = child
                .try_wait()
                .context("could not inspect command status")?;
        }

        if let Some(limit) = memory {
            if !controller.memory_is_native() && usage.rss_bytes > limit {
                eprintln!(
                    "with-limits: memory limit exceeded ({} > {})",
                    format_bytes(usage.rss_bytes),
                    format_bytes(limit)
                );
                stop_command(&mut child, &controller, &mut tree, system, cli.kill_after.0);
                return Ok(CommandResult::Code(EXIT_MEMORY));
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            eprintln!("with-limits: time limit exceeded");
            stop_command(&mut child, &controller, &mut tree, system, cli.kill_after.0);
            return Ok(CommandResult::Code(EXIT_TIMEOUT));
        }

        if let Some(status) = child_status {
            if tree.is_empty() {
                return Ok(CommandResult::Status(status));
            }
        }

        if let Some(cores) = cpu {
            if !controller.cpu_is_native() {
                throttle_cpu(&controller, usage.cpu_percent, cores, cli.poll_interval.0);
            }
        }
        thread::sleep(cli.poll_interval.0);
    }
}

fn throttle_cpu(
    controller: &platform::Controller,
    observed_percent: f64,
    cores: f64,
    interval: Duration,
) {
    let allowed_percent = cores * 100.0;
    if observed_percent <= allowed_percent || allowed_percent == 0.0 {
        return;
    }
    let pause = interval
        .mul_f64(observed_percent / allowed_percent - 1.0)
        .min(Duration::from_secs(1));
    if !pause.is_zero() {
        controller.suspend();
        thread::sleep(pause);
        controller.resume();
    }
}

fn stop_command(
    child: &mut Child,
    controller: &platform::Controller,
    tree: &mut ProcessTree,
    system: &mut System,
    grace: Duration,
) {
    controller.terminate();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        let _ = child.try_wait();
        tree.refresh(system);
        if tree.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(25).min(grace));
    }
    controller.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn status_exit_code(status: ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        ExitCode::from(u8::try_from(code).unwrap_or(EXIT_WRAPPER_ERROR))
    } else if let Some(signal) = status.signal() {
        ExitCode::from(u8::try_from(128 + signal).unwrap_or(EXIT_WRAPPER_ERROR))
    } else {
        ExitCode::from(EXIT_WRAPPER_ERROR)
    }
}

#[cfg(windows)]
fn status_exit_code(status: ExitStatus) -> ExitCode {
    ExitCode::from(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(EXIT_WRAPPER_ERROR),
    )
}

#[allow(dead_code)]
fn display_program(program: &OsStr) -> String {
    program.to_string_lossy().into_owned()
}
