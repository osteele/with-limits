use std::{
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

static RESOURCE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_with-limits"))
}

#[test]
fn preserves_the_command_exit_code() {
    let status = binary()
        .args(["--time", "5s", "-c", exit_command(7)])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
}

#[test]
fn preserves_a_short_commands_status_past_its_deadline() {
    let _guard = resource_test_guard();
    let status = binary()
        .args(["--time", "2s", "--poll-interval", "3s", "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "short_exit_helper"])
        .env("WITH_LIMITS_TEST_SHORT_EXIT", "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
}

#[test]
#[ignore]
fn short_exit_helper() {
    if std::env::var_os("WITH_LIMITS_TEST_SHORT_EXIT").is_some() {
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::process::exit(7);
    }
}

#[test]
#[ignore]
fn memory_hog_helper() {
    if std::env::var_os("WITH_LIMITS_TEST_MEMORY_HOG").is_some() {
        let mut memory = vec![0_u8; 64 * 1024 * 1024];
        for byte in memory.iter_mut().step_by(4096) {
            *byte = 1;
        }
        std::hint::black_box(&memory);
        std::thread::sleep(Duration::from_secs(10));
    }
}

#[cfg(unix)]
#[test]
#[ignore]
fn graceful_termination_helper() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    if let Some(marker) = std::env::var_os("WITH_LIMITS_TEST_MARKER") {
        let terminated = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(libc::SIGTERM, Arc::clone(&terminated)).unwrap();
        while !terminated.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(marker, "terminated").unwrap();
    }
}

#[test]
#[ignore]
fn priority_tree_helper() {
    let Some(expected) = std::env::var_os("WITH_LIMITS_TEST_PRIORITY") else {
        return;
    };
    let expected: i64 = expected.to_string_lossy().parse().unwrap();
    assert_eq!(current_priority(), expected);

    if std::env::var_os("WITH_LIMITS_TEST_PRIORITY_DESCENDANT").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "priority_tree_helper"])
            .env("WITH_LIMITS_TEST_PRIORITY_DESCENDANT", "1")
            .status()
            .unwrap();
        assert!(status.success(), "priority-checking descendant failed");
    }
}

#[test]
fn accepts_combined_resource_limits() {
    let status = binary()
        .args([
            "--memory",
            "auto",
            "--cpu",
            "1",
            "--time",
            "5s",
            "-c",
            exit_command(0),
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn shell_shorthand_uses_the_default_memory_limit() {
    let status = binary().args(["-c", exit_command(0)]).status().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn returns_124_when_time_expires() {
    let _guard = resource_test_guard();
    let started = Instant::now();
    let status = binary()
        .args([
            "--time",
            "100ms",
            "--kill-after",
            "100ms",
            "-c",
            slow_command(),
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(124));
    assert!(started.elapsed().as_secs() < 5);
}

#[test]
fn returns_127_for_a_missing_command() {
    let status = binary()
        .args([
            "--time",
            "1s",
            "--",
            "with-limits-command-that-does-not-exist",
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(127));
}

#[test]
fn returns_125_when_no_command_is_supplied() {
    let output = binary().args(["--time", "1s"]).output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("specify a command after --, or use -c")
    );
}

#[test]
fn returns_clap_usage_status_for_an_invalid_limit() {
    let output = binary()
        .args(["--memory", "101%", "-c", exit_command(0)])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not exceed 100%"));
}

#[cfg(unix)]
#[test]
fn returns_126_for_a_command_that_is_not_executable() {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_path("not-executable");
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = binary()
        .args(["--time", "1s", "--"])
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(output.status.code(), Some(126));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot invoke"));
}

#[test]
fn rejects_native_memory_enforcement_when_only_sampling_is_available() {
    let output = binary()
        .args([
            "--memory",
            "1GiB",
            "--require-native",
            "-c",
            exit_command(0),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("native memory enforcement is unavailable"));
}

#[test]
fn lowers_the_entire_process_tree_priority_by_default() {
    assert_tree_priority(None, default_lower_priority());
}

#[cfg(unix)]
#[test]
fn honors_the_nice_adjustment_override() {
    let current = current_priority();
    assert_tree_priority(Some("5"), current.saturating_add(5).min(19));
}

#[test]
fn allows_priority_lowering_to_be_disabled() {
    assert_tree_priority(Some("off"), current_priority());
}

#[cfg(unix)]
#[test]
fn rejects_native_cpu_enforcement_on_unix() {
    let output = binary()
        .args(["--cpu", "1", "--require-native", "-c", exit_command(0)])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("native CPU enforcement is unavailable")
    );
}

#[test]
fn memory_limit_stops_a_resident_workload() {
    let _guard = resource_test_guard();
    let started = Instant::now();
    let output = binary()
        .args([
            "--memory",
            "24MiB",
            "--time",
            "5s",
            "--kill-after",
            "100ms",
            "--poll-interval",
            "25ms",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "memory_hog_helper"])
        .env("WITH_LIMITS_TEST_MEMORY_HOG", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(137));
    assert!(String::from_utf8_lossy(&output.stderr).contains("memory limit exceeded"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn terminates_a_descendant_that_creates_its_own_session() {
    let _guard = resource_test_guard();
    let pid_file = std::env::temp_dir().join(format!(
        "with-limits-escaped-descendant-{}.pid",
        std::process::id()
    ));
    let script = format!(
        r#"import subprocess,time
p=subprocess.Popen(["python3","-c","import time; time.sleep(20)"], start_new_session=True)
open({:?},"w").write(str(p.pid))
time.sleep(10)"#,
        pid_file
    );
    let status = binary()
        .args([
            "--time",
            "750ms",
            "--kill-after",
            "100ms",
            "--",
            "python3",
            "-c",
            &script,
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(124));

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let survived = unsafe { libc::kill(pid, 0) } == 0;
    if survived {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = std::fs::remove_file(pid_file);
    assert!(!survived, "escaped descendant survived the timeout");
}

#[cfg(unix)]
#[test]
fn requests_graceful_termination_before_forcing_the_tree() {
    let _guard = resource_test_guard();
    let marker = temp_path("graceful-marker");
    let status = binary()
        .args([
            "--time",
            "250ms",
            "--kill-after",
            "2s",
            "--poll-interval",
            "25ms",
            "--",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "graceful_termination_helper"])
        .env("WITH_LIMITS_TEST_MARKER", &marker)
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(124));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "terminated");
    let _ = std::fs::remove_file(marker);
}

#[cfg(unix)]
#[test]
fn forwards_external_termination_signals_to_the_command() {
    let _guard = resource_test_guard();
    let ready = temp_path("signal-ready");
    let marker = temp_path("signal-marker");
    let mut child = binary()
        .args([
            "--time",
            "5s",
            "--kill-after",
            "500ms",
            "--poll-interval",
            "25ms",
            "-c",
            "trap 'printf forwarded > \"$WITH_LIMITS_TEST_MARKER\"; exit 23' TERM; printf ready > \"$WITH_LIMITS_TEST_READY\"; while :; do sleep 1; done",
        ])
        .env("WITH_LIMITS_TEST_READY", &ready)
        .env("WITH_LIMITS_TEST_MARKER", &marker)
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() && Instant::now() < ready_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !ready.exists() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("command did not become ready for signal forwarding");
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(23));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "forwarded");
    let _ = std::fs::remove_file(ready);
    let _ = std::fs::remove_file(marker);
}

#[cfg(windows)]
#[test]
fn preserves_windows_exit_codes_above_255() {
    let status = binary()
        .args(["--time", "5s", "-c", "exit /b 300"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(300));
}

#[cfg(windows)]
#[test]
fn accepts_required_native_cpu_enforcement_on_windows() {
    let status = binary()
        .args([
            "--cpu",
            "1",
            "--require-native",
            "--time",
            "5s",
            "-c",
            exit_command(0),
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}

#[cfg(unix)]
fn exit_command(code: i32) -> &'static str {
    match code {
        0 => "exit 0",
        7 => "exit 7",
        _ => unreachable!(),
    }
}

#[cfg(windows)]
fn exit_command(code: i32) -> &'static str {
    match code {
        0 => "exit /b 0",
        7 => "exit /b 7",
        _ => unreachable!(),
    }
}

#[cfg(unix)]
fn slow_command() -> &'static str {
    "trap '' TERM; sleep 10"
}

#[cfg(windows)]
fn slow_command() -> &'static str {
    "ping -n 10 127.0.0.1 >NUL"
}

fn resource_test_guard() -> std::sync::MutexGuard<'static, ()> {
    RESOURCE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
fn temp_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "with-limits-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn assert_tree_priority(nice: Option<&str>, expected: i64) {
    let mut command = binary();
    command
        .args(["--time", "5s", "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "priority_tree_helper"])
        .env("WITH_LIMITS_TEST_PRIORITY", expected.to_string());
    match nice {
        Some(value) => {
            command.env("WITH_LIMITS_NICE", value);
        }
        None => {
            command.env_remove("WITH_LIMITS_NICE");
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "priority check failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn current_priority() -> i64 {
    i64::from(unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) })
}

#[cfg(unix)]
fn default_lower_priority() -> i64 {
    current_priority().saturating_add(10).min(19)
}

#[cfg(windows)]
fn current_priority() -> i64 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};
    i64::from(unsafe { GetPriorityClass(GetCurrentProcess()) })
}

#[cfg(windows)]
fn default_lower_priority() -> i64 {
    i64::from(windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS)
}
