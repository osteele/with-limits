# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-03

### Added
- Host admission gate: `--check-headroom` reports kernel memory pressure, free memory, swap, and load against a policy and exits `0` or `75`, with `--json` output; `--wait-for-headroom[=DURATION]` polls the same decision with jittered backoff before running the command. Thresholds are set with `--max-pressure`, `--min-memory-free-percent`, `--min-swap-free`, `--max-load-per-cpu`, and `--refuse-unknown`, or their `WITH_LIMITS_*` environment variables
- Cross-process memory reservations: an absolute `--memory` limit publishes a reservation that concurrent admission checks subtract from free memory until the command's tree grows into it; `--no-reservation` opts out and `WITH_LIMITS_RESERVATION_DIR` selects the per-account store
- Independent `--reserve SIZE` admission estimates for commands using `--memory auto` or percentage caps
- `--foreground` keeps the command in the supervisor's process group so it can read from the terminal, as GNU `timeout --foreground` does
- Seed `PYTORCH_MPS_HIGH_WATERMARK_RATIO` and `PYTORCH_MPS_LOW_WATERMARK_RATIO` for guarded trees on macOS, so PyTorch's Metal allocator respects roughly the share the guard enforces; override with `WITH_LIMITS_MPS_*_WATERMARK_RATIO` or disable with `WITH_LIMITS_MPS_WATERMARKS=off`
- Forward `SIGQUIT`, `SIGUSR1`, `SIGUSR2`, `SIGWINCH`, `SIGTSTP`, and `SIGCONT` to Unix command trees according to their native roles

### Changed
- Refuse with exit `75` when an `auto` or percentage memory limit resolves below 256 MiB, rather than starting the command under an unusable cap

### Fixed
- On macOS, read available memory from `kern.memorystatus_level`, so `auto` and percentage caps and the host reserve no longer collapse to a few MiB on a host whose memory compressor is large
- Adopt a descendant that stayed in the command's process group even when its parent exited between two polls, including when the command exits before any poll observes it, so it no longer escapes the limits and the supervisor's exit
- Return the command's status when its process group is left holding only an unreaped zombie, and stop adopting through the group's id once that id has been reused by an unrelated group
- Follow a graceful termination request with `SIGCONT`, so a tree stopped by `SIGTTIN`, `SIGTSTP`, or the CPU throttle receives it instead of waiting for the forced kill
- Force externally signaled Unix process trees to stop after `--kill-after` when they ignore graceful termination
- Name the shared-`/tmp` fallback reservation store by user id, refuse a store that is a symlink, owned by another account, or writable by others, never follow a symlink planted at a record or lock path, and validate a store read by `--check-headroom` after the lock attempt that may create it
- A supervisor's reservation refresh skips a tick instead of waiting when another process holds the reservation store lock, so a stalled holder cannot pause memory and time enforcement
- Check only the reservation processes a scan needs instead of snapshotting the whole process table while holding the store lock
- Keep the headroom backoff jitter strictly below 1.5

## [0.1.0] - 2026-08-23

### Added
- Cross-platform process-tree memory, sustained CPU, and wall-time limits
- Automatic memory limits that preserve a continuously monitored host reserve
- Lower scheduling priority for guarded commands on Unix and Windows
- Graceful termination, signal forwarding, and command exit-status preservation
- Agent hook examples for Claude Code, Codex, OpenCode, and Kimi Code CLI

[Unreleased]: https://github.com/osteele/with-limits/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/osteele/with-limits/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/osteele/with-limits/releases/tag/v0.1.0
