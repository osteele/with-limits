mod platform;
mod process_tree;
mod reservation;

use anyhow::{bail, Context, Result};
use clap::Parser;
use process_tree::ProcessTree;
use reservation::{Reservation, ReservationStore};
use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};
use sysinfo::System;
use with_limits::{
    format_bytes,
    headroom::{self, OutstandingReservations, Policy, Readings, Verdict},
    CpuLimit, HumanDuration, MemorySpec,
};

const EXIT_HEADROOM_REFUSED: u8 = 75; // EX_TEMPFAIL: refused now, try again later
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

    /// Report host headroom and the admission decision, then exit without
    /// running a command
    #[arg(
        long,
        conflicts_with_all = ["shell_command", "command", "wait_for_headroom"]
    )]
    check_headroom: bool,

    /// Emit the --check-headroom report as one JSON object on stdout
    #[arg(long, requires = "check_headroom")]
    json: bool,

    /// Wait for host headroom before starting the command. Without a DURATION
    /// the wait is unbounded; with one, expiry exits 75 without running the
    /// command
    #[arg(long, value_name = "DURATION", num_args = 0..=1, require_equals = true)]
    wait_for_headroom: Option<Option<HumanDuration>>,

    /// Highest admitted kernel memory pressure level (1 normal, 2 warn,
    /// 4 critical)
    #[arg(
        long,
        value_name = "N",
        env = "WITH_LIMITS_MAX_PRESSURE",
        default_value = "2"
    )]
    max_pressure: i32,

    /// Lowest admitted free memory, as a percentage of total. This is the
    /// signal that separates ordinary memory management from exhaustion
    #[arg(
        long,
        value_name = "PERCENT",
        env = "WITH_LIMITS_MIN_MEMORY_FREE_PERCENT",
        default_value = "10",
        value_parser = parse_free_percent
    )]
    min_memory_free_percent: f64,

    /// Lowest admitted free swap once swap has grown past this size. Zero,
    /// the default, reports swap without enforcing it: macOS sizes swap on
    /// demand, so free space within the current files is not exhaustion
    #[arg(
        long,
        value_name = "SIZE",
        env = "WITH_LIMITS_MIN_SWAP_FREE",
        default_value = "0",
        value_parser = parse_swap_floor
    )]
    min_swap_free: u64,

    /// Highest admitted one-minute load average per logical CPU. Load is
    /// reported but not enforced without this flag
    #[arg(
        long,
        value_name = "CORES",
        env = "WITH_LIMITS_MAX_LOAD_PER_CPU",
        value_parser = parse_load_ceiling
    )]
    max_load_per_cpu: Option<f64>,

    /// Treat an unknown enforced signal as a refusal in the headroom forms
    #[arg(long)]
    refuse_unknown: bool,

    /// Memory estimate to publish for admission, independent of --memory
    #[arg(
        long,
        value_name = "SIZE",
        value_parser = parse_reservation_size,
        conflicts_with_all = ["check_headroom", "no_reservation"]
    )]
    reserve: Option<u64>,

    /// Ignore outstanding reservations and do not publish one
    #[arg(long)]
    no_reservation: bool,

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
    if cli.check_headroom {
        return check_headroom(&cli);
    }
    if cli.shell_command.is_none() && cli.command.is_empty() {
        bail!("specify a command after --, or use -c");
    }

    let memory_spec = if cli.memory.is_none() && cli.cpu.is_none() && cli.time.is_none() {
        Some(MemorySpec::AvailableFraction(0.7))
    } else {
        cli.memory
    };
    let reservation_budget = reservation_budget(memory_spec, cli.reserve, cli.no_reservation);
    let reservation_store = (!cli.no_reservation).then(ReservationStore::configured);
    let mut system = System::new_all();
    system.refresh_memory();
    let mut reservation = None;
    if let Some(max_wait) = cli.wait_for_headroom {
        match wait_for_admission(
            &cli,
            &mut system,
            max_wait.map(|limit| limit.0),
            reservation_store.as_ref(),
            reservation_budget,
        )? {
            WaitResult::Admitted(admitted_reservation) => reservation = admitted_reservation,
            WaitResult::Expired => return Ok(CommandResult::Code(EXIT_HEADROOM_REFUSED)),
        }
    } else if let (Some(store), Some(budget)) = (reservation_store.as_ref(), reservation_budget) {
        reservation = Some(store.reserve(budget)?);
    }
    let available = if matches!(memory_spec, Some(MemorySpec::AvailableFraction(_))) {
        available_memory(&mut system)?
    } else {
        system.available_memory()
    };
    let memory_budget = resolve_memory_budget(memory_spec, available)?;
    let memory = memory_budget.process_limit;
    let cpu = cli.cpu.map(|limit| limit.0);
    let nice_adjustment = configured_nice_adjustment()?;
    let mut command = build_command(&cli)?;
    command.env("WITH_LIMITS_ACTIVE", "1");
    for (name, value) in mps_watermarks(&|name| std::env::var_os(name)) {
        command.env(name, value);
    }

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
        memory_budget,
        cpu,
        child,
        controller,
        &mut system,
        reservation,
    )
}

fn reservation_budget(
    spec: Option<MemorySpec>,
    explicit: Option<u64>,
    disabled: bool,
) -> Option<u64> {
    if disabled {
        return None;
    }
    explicit.or(match spec {
        Some(MemorySpec::Bytes(bytes)) => Some(bytes),
        Some(MemorySpec::AvailableFraction(_)) | None => None,
    })
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

fn admission_policy(cli: &Cli) -> Policy {
    Policy {
        max_pressure: cli.max_pressure,
        min_memory_free_percent: (cli.min_memory_free_percent > 0.0)
            .then_some(cli.min_memory_free_percent),
        min_swap_free_bytes: cli.min_swap_free,
        max_load_per_cpu: cli.max_load_per_cpu,
        refuse_unknown: cli.refuse_unknown,
    }
}

fn check_headroom(cli: &Cli) -> Result<CommandResult> {
    let policy = admission_policy(cli);
    let mut system = System::new_all();
    let readings = Readings::collect(&mut system);
    let (verdict, reservations) = if cli.no_reservation {
        (
            headroom::decide(&readings, &policy),
            OutstandingReservations::default(),
        )
    } else {
        ReservationStore::configured().inspect(&readings, &policy)?
    };
    if cli.json {
        print_json_report(&readings, &policy, reservations, &verdict)?;
    } else {
        print_human_report(&readings, &policy, reservations, &verdict);
    }
    Ok(CommandResult::Code(if verdict.admitted {
        0
    } else {
        EXIT_HEADROOM_REFUSED
    }))
}

#[derive(serde::Serialize)]
struct HeadroomReport<'a> {
    readings: &'a Readings,
    policy: &'a Policy,
    reservations: OutstandingReservations,
    refusing: &'a [String],
    unknown: &'a [&'static str],
    admitted: bool,
}

fn print_json_report(
    readings: &Readings,
    policy: &Policy,
    reservations: OutstandingReservations,
    verdict: &Verdict,
) -> Result<()> {
    let report = HeadroomReport {
        readings,
        policy,
        reservations,
        refusing: &verdict.refusing,
        unknown: &verdict.unknown,
        admitted: verdict.admitted,
    };
    let json = serde_json::to_string(&report).context("could not serialize the headroom report")?;
    println!("{json}");
    Ok(())
}

fn print_human_report(
    readings: &Readings,
    policy: &Policy,
    reservations: OutstandingReservations,
    verdict: &Verdict,
) {
    let unknown_outcome = |enforced: bool| {
        if policy.refuse_unknown && enforced {
            "refuses (unknown)"
        } else {
            "reported (unknown)"
        }
    };
    match readings.pressure_level {
        Some(level) => println!(
            "pressure level {level} ({}), maximum {}: {}",
            headroom::pressure_name(level),
            policy.max_pressure,
            if level > policy.max_pressure {
                "refuses"
            } else {
                "ok"
            }
        ),
        None => println!("pressure level unknown: {}", unknown_outcome(true)),
    }
    let effective_memory_free =
        headroom::memory_free_percent_after_reservations(readings, reservations);
    let memory_label = if reservations.unrealized_bytes > 0 {
        "free memory after reservations"
    } else {
        "free memory"
    };
    match effective_memory_free {
        Some(percent) => match policy.min_memory_free_percent {
            Some(floor) => println!(
                "{memory_label} {percent:.0}%, floor {floor:.0}%: {}",
                if percent < floor { "refuses" } else { "ok" }
            ),
            None => println!("{memory_label} {percent:.0}%: reported (no floor set)"),
        },
        None => println!(
            "{memory_label} unknown: {}",
            unknown_outcome(policy.min_memory_free_percent.is_some())
        ),
    }
    match (readings.swap_free_bytes, readings.swap_total_bytes) {
        (Some(free), Some(total)) if policy.min_swap_free_bytes == 0 => println!(
            "swap free {} of {} total: reported (no floor set)",
            format_bytes(free),
            format_bytes(total)
        ),
        (Some(free), Some(total)) if total <= policy.min_swap_free_bytes => println!(
            "swap free {} of {} total, floor {}: ok (swap has not grown past the floor)",
            format_bytes(free),
            format_bytes(total),
            format_bytes(policy.min_swap_free_bytes)
        ),
        (Some(free), Some(total)) => println!(
            "swap free {} of {} total, floor {}: {}",
            format_bytes(free),
            format_bytes(total),
            format_bytes(policy.min_swap_free_bytes),
            if free < policy.min_swap_free_bytes {
                "refuses"
            } else {
                "ok"
            }
        ),
        _ => println!(
            "swap free unknown: {}",
            unknown_outcome(policy.min_swap_free_bytes > 0)
        ),
    }
    match (readings.load_per_cpu, policy.max_load_per_cpu) {
        (Some(load), Some(ceiling)) => println!(
            "load per CPU {load:.2}, ceiling {ceiling}: {}",
            if load > ceiling { "refuses" } else { "ok" }
        ),
        (Some(load), None) => println!("load per CPU {load:.2}: reported (no ceiling set)"),
        (None, ceiling) => println!(
            "load per CPU unknown: {}",
            unknown_outcome(ceiling.is_some())
        ),
    }
    match (readings.available_bytes, readings.available_fraction) {
        (Some(bytes), Some(fraction)) => println!(
            "available memory {} ({:.0}% of total): reported",
            format_bytes(bytes),
            fraction * 100.0
        ),
        _ => println!("available memory unknown: reported"),
    }
    println!(
        "outstanding reservations {} across {} process(es): applied to free memory",
        format_bytes(reservations.unrealized_bytes),
        reservations.count
    );
    if verdict.admitted {
        println!("admitted");
    } else {
        println!("refused: {}", verdict.refusing.join("; "));
    }
}

enum WaitResult {
    Admitted(Option<Reservation>),
    Expired,
}

/// Whether a bounded wait must expire before consulting the host again.
///
/// `has_refused` is what keeps a bound from suppressing an immediate
/// admission: the deadline governs how long the caller waits, so a host with
/// headroom on the first look runs the command whatever the bound. Once a
/// refusal has been observed and slept on, an elapsed bound is decisive, and
/// the command does not run.
fn wait_has_expired(elapsed: Duration, limit: Option<Duration>, has_refused: bool) -> bool {
    match limit {
        Some(limit) => has_refused && elapsed >= limit,
        None => false,
    }
}

/// Poll the admission decision until admitted, then let the caller proceed to
/// run the command. No signal handler is installed during the wait, so a
/// terminating signal ends the process with the default disposition, as it
/// does for any interrupted wrapper.
fn wait_for_admission(
    cli: &Cli,
    system: &mut System,
    max_wait: Option<Duration>,
    reservation_store: Option<&ReservationStore>,
    reservation_budget: Option<u64>,
) -> Result<WaitResult> {
    let policy = admission_policy(cli);
    let started = Instant::now();
    let mut attempt = 0_u32;
    let mut last_notice: Option<Instant> = None;
    // Why the deadline is tested before the reading rather than after the
    // verdict: a bound exists so a caller can rely on "expired" meaning the
    // command did not run. Admitting on the poll that follows an expired
    // sleep would make that promise depend on which side of the deadline the
    // host happened to recover, so the exit status would be racy where the
    // documentation says it is decisive.
    let mut last_refusing: Vec<String> = Vec::new();
    loop {
        if let Some(limit) = max_wait {
            if wait_has_expired(started.elapsed(), Some(limit), !last_refusing.is_empty()) {
                eprintln!(
                    "with-limits: headroom wait expired after {}: {}",
                    HumanDuration(limit),
                    last_refusing.join("; ")
                );
                return Ok(WaitResult::Expired);
            }
        }
        let readings = Readings::collect(system);
        let admission = reservation_store
            .map(|store| store.decide_and_reserve(&readings, &policy, reservation_budget))
            .transpose()?;
        let (verdict, reservation) = match admission {
            Some(admission) => (admission.verdict, admission.reservation),
            None => (headroom::decide(&readings, &policy), None),
        };
        if verdict.admitted {
            if last_notice.is_some() && !cli.quiet {
                eprintln!(
                    "with-limits: headroom admitted after {:.0}s",
                    started.elapsed().as_secs_f64()
                );
            }
            return Ok(WaitResult::Admitted(reservation));
        }
        let elapsed = started.elapsed();
        last_refusing = verdict.refusing.clone();
        if let Some(limit) = max_wait {
            if elapsed >= limit {
                eprintln!(
                    "with-limits: headroom wait expired after {}: {}",
                    HumanDuration(limit),
                    verdict.refusing.join("; ")
                );
                return Ok(WaitResult::Expired);
            }
        }
        if !cli.quiet && last_notice.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            eprintln!(
                "with-limits: waiting for headroom: {}",
                verdict.refusing.join("; ")
            );
            last_notice = Some(Instant::now());
        }
        let mut sleep = headroom::backoff_sleep(attempt, headroom::jitter_sample());
        if let Some(limit) = max_wait {
            sleep = sleep.min(limit.saturating_sub(elapsed));
        }
        thread::sleep(sleep);
        attempt += 1;
    }
}

/// Parse an admission reservation as an absolute memory size.
fn parse_reservation_size(value: &str) -> Result<u64, String> {
    match value.parse::<MemorySpec>()? {
        MemorySpec::Bytes(bytes) => Ok(bytes),
        MemorySpec::AvailableFraction(_) => {
            Err("the reservation takes an absolute size, not a percentage or auto".into())
        }
    }
}

/// Parse the swap floor: an absolute size, or zero to disable the floor.
/// Percentages and `auto` resolve against available memory, which is not the
/// quantity the floor constrains, so they are rejected here.
fn parse_swap_floor(value: &str) -> Result<u64, String> {
    if value.trim() == "0" {
        return Ok(0);
    }
    match value.parse::<MemorySpec>()? {
        MemorySpec::Bytes(bytes) => Ok(bytes),
        MemorySpec::AvailableFraction(_) => {
            Err("the swap floor takes an absolute size, not a percentage or auto".into())
        }
    }
}

fn parse_free_percent(value: &str) -> Result<f64, String> {
    let percent: f64 = value
        .parse()
        .map_err(|_| format!("not a percentage: {value}"))?;
    if !(0.0..=100.0).contains(&percent) {
        return Err(format!("percentage must be between 0 and 100: {value}"));
    }
    Ok(percent)
}

fn parse_load_ceiling(value: &str) -> Result<f64, String> {
    let ceiling = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid load ceiling {value:?}"))?;
    if !ceiling.is_finite() || ceiling <= 0.0 {
        return Err("the load ceiling must be positive".into());
    }
    Ok(ceiling)
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

/// Default PyTorch MPS watermarks for the guarded tree, on macOS.
///
/// A memory ceiling on the process tree does not reach PyTorch's Metal
/// allocator, which sizes its own pool against total system memory and will
/// happily claim past the ceiling: the tree is killed for an allocation the
/// guard never had a chance to refuse. Seeding the watermarks makes the
/// allocator respect roughly the same share the guard does.
///
/// Only defaults. An explicit `PYTORCH_MPS_*` in the environment is the
/// caller's decision and passes through untouched; `WITH_LIMITS_MPS_*`
/// overrides the ratio without having to know PyTorch's variable names, and
/// `WITH_LIMITS_MPS_WATERMARKS=off` disables the seeding entirely.
fn mps_watermarks(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<(&'static str, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let value = |name: &str| var(name).and_then(|v| v.into_string().ok());
    if value("WITH_LIMITS_MPS_WATERMARKS").is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    }) {
        return Vec::new();
    }
    [
        (
            "PYTORCH_MPS_HIGH_WATERMARK_RATIO",
            "WITH_LIMITS_MPS_HIGH_WATERMARK_RATIO",
            "0.7",
        ),
        (
            "PYTORCH_MPS_LOW_WATERMARK_RATIO",
            "WITH_LIMITS_MPS_LOW_WATERMARK_RATIO",
            "0.6",
        ),
    ]
    .into_iter()
    .filter(|(pytorch, _, _)| value(pytorch).is_none())
    .map(|(pytorch, override_name, default)| {
        (
            pytorch,
            value(override_name).unwrap_or_else(|| default.to_string()),
        )
    })
    .collect()
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
    memory_budget: MemoryBudget,
    cpu: Option<f64>,
    mut child: Child,
    mut controller: platform::Controller,
    system: &mut System,
    mut reservation: Option<Reservation>,
) -> Result<CommandResult> {
    let memory = memory_budget.process_limit;
    let host_memory_reserve = memory_budget.host_reserve;
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
    #[cfg(unix)]
    let mut external_termination = None;

    loop {
        #[cfg(unix)]
        if controller.wait_for_forwarded_signal(Duration::ZERO) {
            record_external_termination(&mut external_termination, cli.kill_after.0)?;
        }

        let mut root_exited = false;
        if child_status.is_none() {
            child_status = child
                .try_wait()
                .context("could not inspect command status")?;
            if child_status.is_some() {
                root_exited = true;
            }
        }
        let usage = refresh_tree(&mut tree, system, &mut reservation);
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

        #[cfg(unix)]
        if external_termination.is_some_and(|termination| Instant::now() >= termination.deadline) {
            force_command(&mut child, &controller, &mut tree, system, &mut reservation)?;
            controller.disarm();
            let status = child
                .wait()
                .context("could not collect command after external termination")?;
            return Ok(CommandResult::Status(status));
        }

        #[cfg(unix)]
        if external_termination.is_some() {
            let wait = external_termination
                .map(|termination| {
                    termination
                        .deadline
                        .saturating_duration_since(Instant::now())
                })
                .unwrap_or_default()
                .min(cli.poll_interval.0);
            if controller.wait_for_forwarded_signal(wait) {
                record_external_termination(&mut external_termination, cli.kill_after.0)?;
            }
            continue;
        }

        if let Some(limit) = memory {
            if usage.rss_bytes > limit {
                eprintln!(
                    "with-limits: memory limit exceeded ({} > {})",
                    format_bytes(usage.rss_bytes),
                    format_bytes(limit)
                );
                stop_command(
                    &mut child,
                    &controller,
                    &mut tree,
                    system,
                    &mut reservation,
                    cli.kill_after.0,
                )?;
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
                    stop_command(
                        &mut child,
                        &controller,
                        &mut tree,
                        system,
                        &mut reservation,
                        cli.kill_after.0,
                    )?;
                    controller.disarm();
                    return Ok(CommandResult::Code(EXIT_MEMORY));
                }
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            eprintln!("with-limits: time limit exceeded");
            stop_command(
                &mut child,
                &controller,
                &mut tree,
                system,
                &mut reservation,
                cli.kill_after.0,
            )?;
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
                    refresh_tree(&mut tree, system, &mut reservation);
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
        #[cfg(unix)]
        if controller.wait_for_forwarded_signal(cli.poll_interval.0) {
            record_external_termination(&mut external_termination, cli.kill_after.0)?;
        }
        #[cfg(not(unix))]
        thread::sleep(cli.poll_interval.0);
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct ExternalTermination {
    deadline: Instant,
}

#[cfg(unix)]
fn record_external_termination(
    termination: &mut Option<ExternalTermination>,
    grace: Duration,
) -> Result<()> {
    if termination.is_none() {
        let deadline = Instant::now()
            .checked_add(grace)
            .context("termination grace period is too large")?;
        *termination = Some(ExternalTermination { deadline });
    }
    Ok(())
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

fn refresh_tree(
    tree: &mut ProcessTree,
    system: &mut System,
    reservation: &mut Option<Reservation>,
) -> process_tree::Usage {
    let usage = tree.refresh(system);
    refresh_reservation(reservation, usage.rss_bytes);
    usage
}

fn refresh_reservation(reservation: &mut Option<Reservation>, observed_rss_bytes: u64) {
    refresh_reservation_with(reservation, |reservation| {
        reservation.update(observed_rss_bytes)
    });
}

fn refresh_reservation_with<F>(reservation: &mut Option<Reservation>, update: F)
where
    F: FnOnce(&mut Reservation) -> Result<()>,
{
    let error = reservation
        .as_mut()
        .and_then(|reservation| update(reservation).err());
    if let Some(error) = error {
        eprintln!(
            "with-limits: warning: could not update memory reservation: {error:#}; reservation disabled"
        );
        drop(reservation.take());
    }
}

fn stop_command(
    child: &mut Child,
    controller: &platform::Controller,
    tree: &mut ProcessTree,
    system: &mut System,
    reservation: &mut Option<Reservation>,
    grace: Duration,
) -> Result<()> {
    refresh_tree(tree, system, reservation);
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
        refresh_tree(tree, system, reservation);
        targets = tree.identities();
        controller.set_targets(&targets)?;
        if tree.is_empty() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25).min(grace));
    }
    force_command(child, controller, tree, system, reservation)
}

fn force_command(
    child: &mut Child,
    controller: &platform::Controller,
    tree: &mut ProcessTree,
    system: &mut System,
    reservation: &mut Option<Reservation>,
) -> Result<()> {
    let mut targets = tree.identities();
    controller.set_targets(&targets)?;
    controller.kill(&targets)?;
    let verification_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < verification_deadline {
        child
            .try_wait()
            .context("could not inspect command after forced termination")?;
        refresh_tree(tree, system, reservation);
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

    #[cfg(target_os = "macos")]
    fn watermarks(pairs: &[(&str, &str)]) -> Vec<(&'static str, String)> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
            .collect();
        super::mps_watermarks(&move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_empty_environment_gets_both_default_watermarks() {
        assert_eq!(
            watermarks(&[]),
            vec![
                ("PYTORCH_MPS_HIGH_WATERMARK_RATIO", "0.7".to_string()),
                ("PYTORCH_MPS_LOW_WATERMARK_RATIO", "0.6".to_string()),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_explicit_pytorch_watermark_is_left_alone() {
        // Seeding a default must never overwrite a decision the caller already
        // made, and must not disturb the sibling it did not set.
        assert_eq!(
            watermarks(&[("PYTORCH_MPS_HIGH_WATERMARK_RATIO", "0.95")]),
            vec![("PYTORCH_MPS_LOW_WATERMARK_RATIO", "0.6".to_string())]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_with_limits_override_replaces_the_default_ratio() {
        assert_eq!(
            watermarks(&[("WITH_LIMITS_MPS_HIGH_WATERMARK_RATIO", "0.5")]),
            vec![
                ("PYTORCH_MPS_HIGH_WATERMARK_RATIO", "0.5".to_string()),
                ("PYTORCH_MPS_LOW_WATERMARK_RATIO", "0.6".to_string()),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_seeding_can_be_switched_off() {
        for spelling in ["off", "OFF", "0", "false", "no"] {
            assert!(
                watermarks(&[("WITH_LIMITS_MPS_WATERMARKS", spelling)]).is_empty(),
                "{spelling} should disable watermark seeding"
            );
        }
    }

    #[test]
    fn an_immediate_admission_ignores_the_bound() {
        // The bound governs waiting, not the first look: a host with headroom
        // now runs the command however small the bound.
        assert!(!wait_has_expired(
            Duration::from_secs(10),
            Some(Duration::from_millis(1)),
            false
        ));
    }

    #[test]
    fn a_refused_wait_expires_at_its_bound() {
        assert!(wait_has_expired(
            Duration::from_millis(1),
            Some(Duration::from_millis(1)),
            true
        ));
    }

    #[test]
    fn a_refused_wait_within_its_bound_keeps_waiting() {
        assert!(!wait_has_expired(
            Duration::from_millis(1),
            Some(Duration::from_secs(10)),
            true
        ));
    }

    #[test]
    fn an_unbounded_wait_never_expires() {
        assert!(!wait_has_expired(Duration::from_secs(86_400), None, true));
    }
    use super::*;

    #[test]
    fn a_reservation_refresh_failure_deactivates_and_removes_the_record() {
        let store_path = std::env::temp_dir().join(format!(
            "with-limits-refresh-failure-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        if store_path.exists() {
            std::fs::remove_dir_all(&store_path).expect("remove stale reservation test store");
        }
        let store = ReservationStore::new(store_path.clone());
        let reservation = store.reserve(1_024).expect("publish test reservation");
        let record_path = store_path.join(std::process::id().to_string());
        assert!(record_path.exists());
        let mut reservation = Some(reservation);
        let update_calls = std::cell::Cell::new(0);

        refresh_reservation_with(&mut reservation, |_| {
            update_calls.set(update_calls.get() + 1);
            Err(anyhow::anyhow!("simulated reservation write failure"))
        });
        refresh_reservation_with(&mut reservation, |_| {
            update_calls.set(update_calls.get() + 1);
            Ok(())
        });

        assert_eq!(update_calls.get(), 1);
        assert!(reservation.is_none());
        assert!(!record_path.exists());
        std::fs::remove_dir_all(&store_path).expect("remove reservation test store");
    }

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
    fn explicit_reservations_override_memory_derived_budgets() {
        assert_eq!(
            reservation_budget(Some(MemorySpec::Bytes(4_096)), None, false),
            Some(4_096)
        );
        assert_eq!(
            reservation_budget(Some(MemorySpec::AvailableFraction(0.7)), Some(2_048), false),
            Some(2_048)
        );
        assert_eq!(
            reservation_budget(Some(MemorySpec::Bytes(4_096)), Some(2_048), false),
            Some(2_048)
        );
        assert_eq!(
            reservation_budget(Some(MemorySpec::AvailableFraction(0.7)), None, false),
            None
        );
        assert_eq!(reservation_budget(None, None, false), None);
        assert_eq!(
            reservation_budget(Some(MemorySpec::Bytes(4_096)), Some(2_048), true),
            None
        );
    }

    #[test]
    fn parses_swap_floor_sizes_but_not_fractions() {
        assert_eq!(parse_swap_floor("0").unwrap(), 0);
        assert_eq!(parse_swap_floor("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_swap_floor("1.5 GB").unwrap(), 1_500_000_000);
        for value in ["", "50%", "auto", "-1", "0.5", "1XB"] {
            assert!(
                parse_swap_floor(value).is_err(),
                "unexpectedly accepted {value:?}"
            );
        }
    }

    #[test]
    fn parses_only_absolute_reservation_sizes() {
        assert_eq!(parse_reservation_size("2GiB").unwrap(), 2 << 30);
        for value in ["50%", "auto"] {
            assert!(
                parse_reservation_size(value).is_err(),
                "unexpectedly accepted {value:?}"
            );
        }
    }

    #[test]
    fn parses_positive_load_ceilings() {
        assert_eq!(parse_load_ceiling("0.9").unwrap(), 0.9);
        assert_eq!(parse_load_ceiling(" 4 ").unwrap(), 4.0);
        for value in ["", "0", "-1", "abc", "NaN", "inf"] {
            assert!(
                parse_load_ceiling(value).is_err(),
                "unexpectedly accepted {value:?}"
            );
        }
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
