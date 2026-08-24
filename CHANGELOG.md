# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
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
