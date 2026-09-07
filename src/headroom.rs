//! Host headroom readings and the admission decision built on them.
//!
//! `with-limits` contains a process tree after it starts; this module answers
//! the earlier question of whether to start one at all. Every reading is an
//! `Option`: a platform that cannot answer, a sysctl that fails, and an
//! unparsable file each yield `None` for that signal only, and `None` is never
//! replaced by a fabricated value.

use serde::Serialize;
use std::time::Duration;
use sysinfo::System;

/// Snapshot of the host signals the admission policy consults.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Readings {
    /// Kernel memory pressure level: 1 normal, 2 warn, 4 critical. Read on
    /// macOS from `kern.memorystatus_vm_pressure_level`. On Linux it is
    /// derived from PSI `/proc/pressure/memory`, which is a mapping onto the
    /// macOS scale, not a kernel verdict. Unknown on Windows.
    pub pressure_level: Option<i32>,
    pub swap_free_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    /// One-minute load average divided by the number of logical CPUs.
    /// Unknown on Windows.
    pub load_per_cpu: Option<f64>,
    pub available_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    /// Available memory as a fraction of total memory.
    pub available_fraction: Option<f64>,
    /// Free memory as a percentage of total. On macOS this is the kernel's own
    /// `kern.memorystatus_level`; elsewhere it is derived from available
    /// memory. It is the signal that separates a host doing ordinary memory
    /// management from one about to run out: a workstation running many agent
    /// sessions sits at warn pressure for hours with a third of memory free,
    /// while the state this gate exists to refuse had 32 MB free.
    pub memory_free_percent: Option<f64>,
}

impl Readings {
    /// Read every signal the platform offers, refreshing `system`'s memory
    /// counters. A failure of one signal leaves that signal `None` and does
    /// not affect the others.
    pub fn collect(system: &mut System) -> Self {
        system.refresh_memory();
        let available = system.available_memory();
        let total = system.total_memory();
        Self {
            pressure_level: kernel_pressure_level(),
            swap_free_bytes: Some(system.free_swap()),
            swap_total_bytes: Some(system.total_swap()),
            load_per_cpu: load_per_cpu(system),
            available_bytes: (available > 0).then_some(available),
            total_bytes: (total > 0).then_some(total),
            available_fraction: (available > 0 && total > 0)
                .then(|| available as f64 / total as f64),
            memory_free_percent: memory_free_percent(available, total),
        }
    }
}

/// The thresholds a host must satisfy to admit new work.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Policy {
    /// Refuse when the pressure level exceeds this. Levels are 1, 2, and 4.
    /// The default of 2 refuses only a host the kernel calls critical: warn is
    /// the ordinary operating state of a machine running many concurrent
    /// tasks, so refusing it blocks work for hours without reducing risk.
    pub max_pressure: i32,
    /// Refuse when free memory falls below this percentage of total. `None`
    /// reports the percentage without enforcing it.
    pub min_memory_free_percent: Option<f64>,
    /// Refuse when swap free falls below this floor, once swap has grown past
    /// it. Zero, the default, reports swap without enforcing it: macOS sizes
    /// swap on demand and grows it while free disk allows, so free space
    /// within the current swap files does not measure exhaustion.
    pub min_swap_free_bytes: u64,
    /// Refuse when load per CPU exceeds this ceiling. `None` reports load
    /// without enforcing it.
    pub max_load_per_cpu: Option<f64>,
    /// Treat an unknown enforced signal as a refusal. A signal is enforced
    /// when its threshold is set: pressure always, free memory and swap and
    /// load when their floors or ceiling are.
    pub refuse_unknown: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_pressure: 2,
            min_memory_free_percent: Some(10.0),
            min_swap_free_bytes: 0,
            max_load_per_cpu: None,
            refuse_unknown: false,
        }
    }
}

/// The outcome of applying a policy to one set of readings.
#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub admitted: bool,
    /// Every reason that refuses, in human-readable form.
    pub refusing: Vec<String>,
    /// Names of signals the platform could not read.
    pub unknown: Vec<&'static str>,
}

/// Reservations that reduce the memory available for admission.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct OutstandingReservations {
    pub count: usize,
    pub unrealized_bytes: u64,
}

/// Apply `policy` to `readings`. A signal the platform could not read is
/// named in `unknown`; it refuses only when the policy asks for that and the
/// signal is enforced.
pub fn decide(readings: &Readings, policy: &Policy) -> Verdict {
    decide_with_reservations(readings, policy, OutstandingReservations::default())
}

/// Apply `policy` after subtracting the unrealized part of live memory
/// reservations from the host's free-memory signal.
pub fn decide_with_reservations(
    readings: &Readings,
    policy: &Policy,
    reservations: OutstandingReservations,
) -> Verdict {
    let mut refusing = Vec::new();
    let mut unknown = Vec::new();

    match readings.pressure_level {
        Some(level) => {
            if level > policy.max_pressure {
                refusing.push(format!(
                    "kernel memory pressure level {level} ({}) exceeds the maximum {}",
                    pressure_name(level),
                    policy.max_pressure
                ));
            }
        }
        None => {
            unknown.push("pressure");
            if policy.refuse_unknown {
                refusing.push("kernel memory pressure is unknown".into());
            }
        }
    }

    let adjusted_memory_free_percent =
        memory_free_percent_after_reservations(readings, reservations);
    match adjusted_memory_free_percent {
        Some(percent) => {
            if let Some(floor) = policy.min_memory_free_percent {
                if percent < floor {
                    if reservations.unrealized_bytes > 0
                        && readings
                            .memory_free_percent
                            .is_some_and(|unadjusted| unadjusted >= floor)
                    {
                        refusing.push(format!(
                            "{} in {} outstanding reservation(s) reduce free memory to {percent:.0}%, below the floor {floor:.0}%",
                            crate::format_bytes(reservations.unrealized_bytes),
                            reservations.count
                        ));
                    } else {
                        refusing.push(format!(
                            "free memory {percent:.0}% is below the floor {floor:.0}%"
                        ));
                    }
                }
            }
        }
        None => {
            unknown.push("free memory");
            if policy.refuse_unknown && policy.min_memory_free_percent.is_some() {
                refusing.push("free memory is unknown".into());
            }
        }
    }

    match (readings.swap_free_bytes, readings.swap_total_bytes) {
        (Some(free), Some(total)) => {
            // macOS allocates swap lazily and grows it in 1 GiB files on
            // demand, so a total of zero is swap that was never needed and a
            // total at or below the floor is swap the system has barely
            // touched; neither is exhaustion. The floor measures headroom
            // only once swap has grown past it, and the pressure signal
            // guards the interval before that.
            if total > policy.min_swap_free_bytes && free < policy.min_swap_free_bytes {
                refusing.push(format!(
                    "swap free {} is below the floor {}",
                    crate::format_bytes(free),
                    crate::format_bytes(policy.min_swap_free_bytes)
                ));
            }
        }
        _ => {
            unknown.push("swap");
            // Gated on the floor for the same reason free memory and load are:
            // refuse-on-unknown applies to a signal this policy enforces, and
            // a floor of zero says swap is reported rather than enforced.
            // Refusing here would reject a host over a signal the same run
            // reports as advisory.
            if policy.refuse_unknown && policy.min_swap_free_bytes > 0 {
                refusing.push("swap headroom is unknown".into());
            }
        }
    }

    match readings.load_per_cpu {
        Some(load) => {
            if let Some(ceiling) = policy.max_load_per_cpu {
                if load > ceiling {
                    refusing.push(format!(
                        "load per CPU {load:.2} exceeds the ceiling {ceiling}"
                    ));
                }
            }
        }
        None => {
            unknown.push("load");
            if policy.refuse_unknown && policy.max_load_per_cpu.is_some() {
                refusing.push("load per CPU is unknown".into());
            }
        }
    }

    if readings.available_bytes.is_none() && readings.memory_free_percent.is_none() {
        unknown.push("available memory");
    }

    Verdict {
        admitted: refusing.is_empty(),
        refusing,
        unknown,
    }
}

/// Return the free-memory signal after subtracting unrealized reservations.
pub fn memory_free_percent_after_reservations(
    readings: &Readings,
    reservations: OutstandingReservations,
) -> Option<f64> {
    let percent = readings.memory_free_percent?;
    if reservations.unrealized_bytes == 0 {
        return Some(percent);
    }
    let total = readings.total_bytes?;
    let free_bytes = (percent / 100.0 * total as f64) as u64;
    Some(free_bytes.saturating_sub(reservations.unrealized_bytes) as f64 / total as f64 * 100.0)
}

/// Name a macOS pressure level for display.
pub fn pressure_name(level: i32) -> &'static str {
    match level {
        1 => "normal",
        2 => "warn",
        4 => "critical",
        _ => "unknown",
    }
}

#[cfg(target_os = "macos")]
fn kernel_pressure_level() -> Option<i32> {
    let name = b"kern.memorystatus_vm_pressure_level\0";
    let mut level: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&level);
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            &mut level as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0).then_some(level)
}

/// Free memory as a percentage of total.
///
/// macOS reports it directly as `kern.memorystatus_level`, which is the
/// counter the kernel's own pressure logic watches. Elsewhere it is derived
/// from the available and total figures sysinfo provides, and is unknown when
/// either is zero.
#[cfg(target_os = "macos")]
fn memory_free_percent(_available: u64, _total: u64) -> Option<f64> {
    let name = b"kern.memorystatus_level\0";
    let mut level: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&level);
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            &mut level as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0).then_some(f64::from(level))
}

#[cfg(not(target_os = "macos"))]
fn memory_free_percent(available: u64, total: u64) -> Option<f64> {
    (available > 0 && total > 0).then(|| available as f64 / total as f64 * 100.0)
}

/// Map the PSI memory `some avg10` stall percentage onto the macOS pressure
/// scale. avg10 is the share of wall time over the last ten seconds in which
/// at least one task stalled on memory: below 5% the host keeps up (normal),
/// 5-25% means work is visibly waiting (warn), and at or above 25% a quarter
/// of all time is lost to memory stalls (critical). This is a mapping chosen
/// for this tool, not a kernel-reported level.
#[cfg(any(target_os = "linux", test))]
fn psi_pressure_level(avg10: f64) -> i32 {
    if avg10 < 5.0 {
        1
    } else if avg10 < 25.0 {
        2
    } else {
        4
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_psi_memory_level(content: &str) -> Option<i32> {
    let avg10 = content
        .lines()
        .find(|line| line.starts_with("some "))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("avg10="))
        })
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)?;
    Some(psi_pressure_level(avg10))
}

#[cfg(target_os = "linux")]
fn kernel_pressure_level() -> Option<i32> {
    let content = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    parse_psi_memory_level(&content)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel_pressure_level() -> Option<i32> {
    None
}

#[cfg(unix)]
fn load_per_cpu(system: &System) -> Option<f64> {
    let one = System::load_average().one;
    if !one.is_finite() {
        return None;
    }
    let cpus = system.cpus().len();
    if cpus == 0 {
        return None;
    }
    Some(one / cpus as f64)
}

#[cfg(not(unix))]
fn load_per_cpu(_system: &System) -> Option<f64> {
    None
}

const WAIT_BASE: Duration = Duration::from_secs(15);
const WAIT_CAP: Duration = Duration::from_secs(90);

/// The sleep before attempt `attempt + 1` of the headroom wait:
/// `base * 1.5^attempt * jitter`, with `jitter` in `[0.5, 1.5)`, capped at 90
/// seconds. The jitter decorrelates the roughly thirty agent sessions that
/// poll this host independently; a fixed backoff would make them retry in
/// lockstep and start together the moment pressure clears.
pub fn backoff_sleep(attempt: u32, jitter: f64) -> Duration {
    debug_assert!((0.5..1.5).contains(&jitter));
    let base = WAIT_BASE.as_secs_f64() * 1.5_f64.powi(attempt.min(64) as i32);
    Duration::from_secs_f64((base * jitter).min(WAIT_CAP.as_secs_f64()))
}

/// A jitter sample in `[0.5, 1.5)`. The seed mixes the clock, the process id,
/// and a per-process counter; it decorrelates pollers and is not a source of
/// randomness for any security purpose.
pub fn jitter_sample() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let mut mixed = u64::from(nanos)
        ^ (u64::from(std::process::id()) << 32)
        ^ COUNTER.fetch_add(0x9e37_79b9, Ordering::Relaxed);
    mixed ^= mixed >> 33;
    mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
    mixed ^= mixed >> 33;
    0.5 + (mixed as f64 / u64::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that enforces the swap floor, for the tests whose subject is
    /// the floor itself. The default policy reports swap without enforcing it.
    fn swap_enforced() -> Policy {
        Policy {
            min_swap_free_bytes: 1 << 30,
            ..Policy::default()
        }
    }

    fn healthy() -> Readings {
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

    #[test]
    fn admits_a_healthy_host() {
        let verdict = decide(&healthy(), &Policy::default());
        assert!(verdict.admitted);
        assert!(verdict.refusing.is_empty());
        assert!(verdict.unknown.is_empty());
    }

    #[test]
    fn refuses_on_pressure_alone() {
        let readings = Readings {
            pressure_level: Some(4),
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("pressure level 4 (critical)"));
    }

    #[test]
    fn admits_warn_pressure() {
        // Warn is the ordinary state of a machine running many agent sessions;
        // refusing it parks work for hours without reducing risk.
        let readings = Readings {
            pressure_level: Some(2),
            ..healthy()
        };
        assert!(decide(&readings, &Policy::default()).admitted);
    }

    #[test]
    fn refuses_on_free_memory_alone() {
        let readings = Readings {
            memory_free_percent: Some(3.0),
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("free memory 3%"));
    }

    #[test]
    fn admits_the_free_memory_a_busy_workstation_reports() {
        // Every refusal observed on this machine sat between 27% and 53%.
        let readings = Readings {
            pressure_level: Some(2),
            memory_free_percent: Some(34.0),
            swap_free_bytes: Some(376 << 20),
            swap_total_bytes: Some(2 << 30),
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(verdict.admitted, "refused with {:?}", verdict.refusing);
    }

    #[test]
    fn refuses_the_state_the_gate_was_built_for() {
        // 2026-09-05: 32 MB free of memory, swap 21205 MB used of 22528.
        let readings = Readings {
            pressure_level: Some(4),
            memory_free_percent: Some(0.2),
            swap_free_bytes: Some(1323 << 20),
            swap_total_bytes: Some(22528 << 20),
            load_per_cpu: Some(2.7),
            available_bytes: Some(32 << 20),
            total_bytes: Some(16 << 30),
            available_fraction: Some(0.002),
        };
        assert!(!decide(&readings, &Policy::default()).admitted);
    }

    #[test]
    fn reports_free_memory_without_enforcing_it_when_no_floor_is_set() {
        let readings = Readings {
            memory_free_percent: Some(1.0),
            ..healthy()
        };
        let policy = Policy {
            min_memory_free_percent: None,
            ..Policy::default()
        };
        assert!(decide(&readings, &policy).admitted);
    }

    #[test]
    fn subtracts_unrealized_reservations_from_free_memory() {
        let readings = Readings {
            memory_free_percent: Some(50.0),
            total_bytes: Some(10_000),
            ..healthy()
        };
        let policy = Policy {
            min_memory_free_percent: Some(30.0),
            ..Policy::default()
        };

        let at_floor = decide_with_reservations(
            &readings,
            &policy,
            OutstandingReservations {
                count: 2,
                unrealized_bytes: 2_000,
            },
        );
        assert!(at_floor.admitted, "the memory floor is inclusive");

        let below_floor = decide_with_reservations(
            &readings,
            &policy,
            OutstandingReservations {
                count: 2,
                unrealized_bytes: 2_001,
            },
        );
        assert!(!below_floor.admitted);
        assert_eq!(below_floor.refusing.len(), 1);
        assert!(below_floor.refusing[0].contains("outstanding reservation(s)"));
        assert!(below_floor.refusing[0].contains("2.0 KiB"));
    }

    #[test]
    fn reservation_subtraction_saturates_at_zero_free_memory() {
        let readings = Readings {
            memory_free_percent: Some(20.0),
            total_bytes: Some(10_000),
            ..healthy()
        };
        let policy = Policy {
            min_memory_free_percent: Some(1.0),
            ..Policy::default()
        };

        let verdict = decide_with_reservations(
            &readings,
            &policy,
            OutstandingReservations {
                count: 1,
                unrealized_bytes: 3_000,
            },
        );
        assert!(!verdict.admitted);
        assert!(verdict.refusing[0].contains("free memory to 0%"));
    }

    #[test]
    fn reservation_adjustment_is_unknown_without_total_memory() {
        let readings = Readings {
            total_bytes: None,
            ..healthy()
        };
        let policy = Policy {
            refuse_unknown: true,
            ..Policy::default()
        };

        let verdict = decide_with_reservations(
            &readings,
            &policy,
            OutstandingReservations {
                count: 1,
                unrealized_bytes: 1,
            },
        );
        assert!(!verdict.admitted);
        assert_eq!(verdict.unknown, ["free memory"]);
        assert_eq!(verdict.refusing, ["free memory is unknown"]);
    }

    #[test]
    fn refuses_on_swap_alone() {
        let readings = Readings {
            swap_free_bytes: Some(512 << 20),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("swap free 512.0 MiB"));
    }

    #[test]
    fn reports_swap_without_enforcing_it_by_default() {
        // Free space inside the current swap files is not exhaustion: macOS
        // grows swap while free disk allows.
        let readings = Readings {
            swap_free_bytes: Some(0),
            swap_total_bytes: Some(4 << 30),
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(verdict.admitted, "refused with {:?}", verdict.refusing);
    }

    #[test]
    fn refuses_on_load_alone_when_a_ceiling_is_set() {
        let readings = Readings {
            load_per_cpu: Some(3.0),
            ..healthy()
        };
        let policy = Policy {
            max_load_per_cpu: Some(1.0),
            ..Policy::default()
        };
        let verdict = decide(&readings, &policy);
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("load per CPU 3.00"));
    }

    #[test]
    fn reports_every_refusing_reason() {
        let readings = Readings {
            pressure_level: Some(4),
            swap_free_bytes: Some(0),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 2);
        assert!(verdict.refusing[0].contains("pressure level 4 (critical)"));
        assert!(verdict.refusing[1].contains("swap free 0 B"));
    }

    #[test]
    fn exempts_zero_swap_total_from_the_floor() {
        let readings = Readings {
            swap_free_bytes: Some(0),
            swap_total_bytes: Some(0),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(verdict.admitted, "refused with {:?}", verdict.refusing);
    }

    #[test]
    fn exempts_a_swap_total_within_the_floor() {
        // macOS grows swap in 1 GiB files: a 1 GiB total with 600 MiB free is
        // a system that has swapped a few hundred MiB, not one that is out.
        let readings = Readings {
            swap_free_bytes: Some(600 << 20),
            swap_total_bytes: Some(1 << 30),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(verdict.admitted, "refused with {:?}", verdict.refusing);
    }

    #[test]
    fn refuses_low_free_swap_once_swap_has_grown_past_the_floor() {
        let readings = Readings {
            swap_free_bytes: Some(600 << 20),
            swap_total_bytes: Some(4 << 30),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(!verdict.admitted);
    }

    #[test]
    fn pressure_guards_the_zero_swap_total_interval() {
        let readings = Readings {
            pressure_level: Some(4),
            swap_free_bytes: Some(0),
            swap_total_bytes: Some(0),
            ..healthy()
        };
        let verdict = decide(&readings, &swap_enforced());
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("pressure"));
    }

    #[test]
    fn reports_unknown_pressure_without_refusing() {
        let readings = Readings {
            pressure_level: None,
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(verdict.admitted);
        assert_eq!(verdict.unknown, ["pressure"]);
    }

    #[test]
    fn unknown_swap_does_not_refuse_while_the_floor_is_disabled() {
        // The default reports swap without enforcing it, so an unreadable swap
        // signal is not a refusal even under --refuse-unknown.
        let readings = Readings {
            swap_free_bytes: None,
            swap_total_bytes: None,
            ..healthy()
        };
        let policy = Policy {
            refuse_unknown: true,
            ..Policy::default()
        };
        let verdict = decide(&readings, &policy);
        assert!(verdict.admitted, "refused with {:?}", verdict.refusing);
        assert!(verdict.unknown.contains(&"swap"));
    }

    #[test]
    fn unknown_swap_refuses_once_the_floor_is_set() {
        let readings = Readings {
            swap_free_bytes: None,
            swap_total_bytes: None,
            ..healthy()
        };
        let policy = Policy {
            refuse_unknown: true,
            ..swap_enforced()
        };
        let verdict = decide(&readings, &policy);
        assert!(!verdict.admitted);
        assert!(verdict
            .refusing
            .iter()
            .any(|r| r.contains("swap headroom is unknown")));
    }

    #[test]
    fn refuse_unknown_turns_an_unknown_enforced_signal_into_a_refusal() {
        let readings = Readings {
            pressure_level: None,
            ..healthy()
        };
        let policy = Policy {
            refuse_unknown: true,
            ..Policy::default()
        };
        let verdict = decide(&readings, &policy);
        assert!(!verdict.admitted);
        assert_eq!(verdict.refusing.len(), 1);
        assert!(verdict.refusing[0].contains("pressure is unknown"));
    }

    #[test]
    fn reports_load_without_enforcing_it_by_default() {
        let readings = Readings {
            load_per_cpu: Some(100.0),
            ..healthy()
        };
        let verdict = decide(&readings, &Policy::default());
        assert!(verdict.admitted);
        assert!(verdict.unknown.is_empty());
    }

    #[test]
    fn unknown_load_refuses_only_when_enforced() {
        let readings = Readings {
            load_per_cpu: None,
            ..healthy()
        };
        let policy = Policy {
            refuse_unknown: true,
            ..Policy::default()
        };
        let verdict = decide(&readings, &policy);
        assert!(verdict.admitted, "load is not enforced without a ceiling");
        assert_eq!(verdict.unknown, ["load"]);

        let policy = Policy {
            max_load_per_cpu: Some(1.0),
            ..policy
        };
        let verdict = decide(&readings, &policy);
        assert!(!verdict.admitted);
        assert!(verdict.refusing[0].contains("load per CPU is unknown"));
    }

    #[test]
    fn maps_psi_stall_time_onto_pressure_levels() {
        assert_eq!(psi_pressure_level(0.0), 1);
        assert_eq!(psi_pressure_level(4.99), 1);
        assert_eq!(psi_pressure_level(5.0), 2);
        assert_eq!(psi_pressure_level(24.99), 2);
        assert_eq!(psi_pressure_level(25.0), 4);
        assert_eq!(psi_pressure_level(80.0), 4);
    }

    #[test]
    fn parses_psi_memory_files() {
        let content =
            "some avg10=3.50 avg60=1.20 avg300=0.40 total=12345\nfull avg10=0.10 avg60=0.05 avg300=0.01 total=6789\n";
        assert_eq!(parse_psi_memory_level(content), Some(1));
        let content = "some avg10=40.00 avg60=20.0 avg300=5.0 total=1\n";
        assert_eq!(parse_psi_memory_level(content), Some(4));
        for content in [
            "",
            "full avg10=0.10 avg60=0.05 avg300=0.01 total=6789\n",
            "some avg10=later avg60=0.05 avg300=0.01 total=6789\n",
            "some avg10=-1.0 avg60=0.05 avg300=0.01 total=6789\n",
        ] {
            assert_eq!(
                parse_psi_memory_level(content),
                None,
                "unexpectedly parsed {content:?}"
            );
        }
    }

    #[test]
    fn backoff_grows_within_the_jitter_bounds() {
        for (attempt, base) in [(0, 15.0), (1, 22.5), (2, 33.75)] {
            assert_eq!(
                backoff_sleep(attempt, 0.5),
                Duration::from_secs_f64(base * 0.5)
            );
            assert_eq!(
                backoff_sleep(attempt, 1.49),
                Duration::from_secs_f64(base * 1.49)
            );
        }
    }

    #[test]
    fn backoff_caps_each_sleep_at_ninety_seconds() {
        // 15 * 1.5^4 * 1.5 = 113.9, so the cap binds from attempt 4 upward.
        for attempt in [4, 5, 10, 64, 65, 1_000_000] {
            assert_eq!(backoff_sleep(attempt, 1.49), WAIT_CAP);
        }
        assert!(backoff_sleep(3, 1.49) < WAIT_CAP);
    }

    #[test]
    fn backoff_total_is_bounded_by_the_cap_times_attempts() {
        for attempts in [1, 5, 20] {
            let total: Duration = (0..attempts).map(|a| backoff_sleep(a, 1.49)).sum();
            assert!(total <= WAIT_CAP * attempts);
            let floor: Duration = (0..attempts).map(|a| backoff_sleep(a, 0.5)).sum();
            assert!(floor <= total);
        }
    }

    #[test]
    fn jitter_samples_stay_in_range() {
        for _ in 0..100 {
            let sample = jitter_sample();
            assert!((0.5..1.5).contains(&sample), "jitter {sample} out of range");
        }
    }
}
