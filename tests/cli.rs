use std::{process::Command, time::Instant};

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

#[cfg(unix)]
#[test]
fn terminates_a_descendant_that_creates_its_own_session() {
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

#[cfg(windows)]
#[test]
fn preserves_windows_exit_codes_above_255() {
    let status = binary()
        .args(["--time", "5s", "-c", "exit /b 300"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(300));
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
