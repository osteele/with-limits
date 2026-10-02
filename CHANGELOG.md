# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Seed `PYTORCH_MPS_HIGH_WATERMARK_RATIO` and `PYTORCH_MPS_LOW_WATERMARK_RATIO` for guarded trees on macOS, so PyTorch's Metal allocator respects roughly the share the guard enforces; override with `WITH_LIMITS_MPS_*_WATERMARK_RATIO` or disable with `WITH_LIMITS_MPS_WATERMARKS=off`
- Forward `SIGQUIT`, `SIGUSR1`, `SIGUSR2`, `SIGWINCH`, `SIGTSTP`, and `SIGCONT` to Unix command trees according to their native roles
- Independent `--reserve SIZE` admission estimates for commands using `--memory auto` or percentage caps

### Fixed
- On macOS, read available memory from `kern.memorystatus_level`, so `auto` and percentage caps and the host reserve no longer collapse to a few MiB on a host whose memory compressor is large
- Refuse with exit `75` when an `auto` or percentage memory limit resolves below 256 MiB, rather than starting the command under an unusable cap
- Force externally signaled Unix process trees to stop after `--kill-after` when they ignore graceful termination

## [0.1.0] - 2026-08-23

### Added
- Cross-platform process-tree memory, sustained CPU, and wall-time limits
- Automatic memory limits that preserve a continuously monitored host reserve
- Lower scheduling priority for guarded commands on Unix and Windows
- Graceful termination, signal forwarding, and command exit-status preservation
- Agent hook examples for Claude Code, Codex, OpenCode, and Kimi Code CLI

[Unreleased]: https://github.com/osteele/with-limits/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/osteele/with-limits/releases/tag/v0.1.0
