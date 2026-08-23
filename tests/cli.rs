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
