# with-limits

[![Crates.io](https://img.shields.io/crates/v/with-limits.svg)](https://crates.io/crates/with-limits)
[![CI](https://github.com/osteele/with-limits/actions/workflows/ci.yml/badge.svg)](https://github.com/osteele/with-limits/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

`with-limits` keeps a development machine responsive while background commands
run. It gives foreground applications scheduling priority, limits the child
process tree's memory, sustained CPU use, and wall-clock runtime, and works on
macOS, Windows, and Linux.

The motivating case is concurrent agents doing machine-learning or research
work. Several agents can each start a RAM-intensive job that looks reasonable
in isolation, while their combined memory use exhausts and crashes the host.
Putting a process-tree limit around each job contains that failure.

The entire child tree also runs at a fixed lower scheduling priority. On Unix,
its inherited niceness is increased by 10, capped at the maximum of 19. Windows
uses Below Normal priority. This still lets a job use otherwise-idle CPU
capacity while allowing interactive applications to win when they need it.

For a time limit alone, GNU `timeout` (installed as `gtimeout` by Homebrew) is
the established choice. `with-limits` is useful when one portable command
should also constrain memory or sustained CPU consumption.

## Status

`with-limits` is an early-stage, actively maintained tool. Releases before 1.0
may change command-line options, environment variables, and exit-status
behavior.

## Installation

Installation requires Rust 1.85 or newer and Cargo. The
[Rust toolchain installer](https://rustup.rs/) provides both.

Install from [crates.io](https://crates.io/crates/with-limits):

```sh
cargo install with-limits --locked
```

This installs `with-limits` in Cargo's binary directory, normally
`~/.cargo/bin`. Ensure that directory is on your `PATH`.

### Build from Source

```sh
git clone https://github.com/osteele/with-limits.git
cd with-limits
cargo install --path .
```

`with-limits` is intended for trusted local and CI workloads. It is a resource
guard, not a security sandbox for hostile code.

## Usage

Verify the installation with a command available alongside Cargo:

```sh
with-limits --time 5s -- cargo --version
```

A successful run prints the Cargo version and exits with status zero. In an
interactive terminal, the startup summary also reports the time limit and
lower scheduling priority.

Place the command after `--`:

```sh
with-limits --memory 8GiB --cpu 2 --time 30m -- python train.py
```

Use `-c` to run a shell command. On macOS this uses `/bin/zsh`; on other Unix
systems it uses `/bin/sh`; and on Windows it uses `%COMSPEC%`, falling back to
`cmd.exe` when that variable is unset.

```sh
with-limits --memory 4GiB -c 'just format && just check'
```

When no limit option is supplied, `with-limits` applies `--memory auto`, which
is 70% of the memory available when the command starts. It also preserves the
remaining 30% as a continuously checked host reserve. This lets several
concurrent guarded agents react to their combined memory use:

```sh
with-limits -c 'just format && just check'
```

Lower scheduling priority is independent of the resource-limit options and is
enabled for every command by default.

Options:

- `--memory SIZE`, `-m SIZE` limits aggregate resident memory. Sizes accept SI
  suffixes such as `GB`, IEC suffixes such as `GiB`, `auto`, or a percentage of
  initially available memory such as `60%`.
- `--cpu CORES` limits sustained CPU use, where `1` is the capacity of one
  logical core. Fractional values such as `0.5` are accepted; the minimum is
  `0.01`.
- `--time DURATION`, `-t DURATION` limits wall-clock runtime. Durations use the
  GNU `timeout` suffixes `s`, `m`, `h`, and `d`; milliseconds use `ms`.
- `--kill-after DURATION` (also `--grace`) controls how long graceful
  termination may take before the process tree is forcibly terminated. The
  default is two seconds.
- `--shell PATH` selects the shell used by `-c`.
- `--require-native` rejects a requested memory or CPU limit if the platform
  would enforce it by sampling. Memory limits are currently sampled on every
  platform; Windows CPU limits are native.
- `--quiet`, `-q` suppresses the interactive startup summary. Limit violations
  are still reported.

## Agent hooks

Agent lifecycle hooks can place selected shell workloads under `with-limits`
without changing the command the model generates. The included example wraps
POSIX `uv run` requests, including the rest of a pipeline or compound command,
under one default memory budget:

```text
uv run python experiment.py && just process-results
→ with-limits -c 'uv run python experiment.py && just process-results'
```

| Agent | Automatic wrapping path |
| --- | --- |
| Claude Code | Native `PreToolUse` command hook |
| Codex | Native `PreToolUse` command hook |
| OpenCode | `tool.execute.before` plugin |
| Kimi Code CLI | Session-wide launcher or command-specific PATH wrapper |

Claude Code and Codex can use the same hook script. OpenCode reaches it through
a small plugin. Kimi hooks cannot currently replace a tool input, so they cannot
apply this command-by-command rewrite. See [Agent shell hooks](docs/agent-hooks.md)
for the example, configuration, security tradeoff, and platform scope.

## Environment

`WITH_LIMITS_NICE` controls fixed priority lowering. It defaults to `10`. On
Unix, values from `1` through `19` are added to the child process's inherited
niceness, capped at 19; descendants inherit the result. On Windows, any enabled
value selects the Below Normal priority class for the Job Object and therefore
its entire process tree. Set it to `0`, `off`, `false`, or `no` to disable
priority lowering.

The priority does not change in response to load or the number of running
agents. Fixed lower priority lets foreground work preempt background jobs while
leaving idle CPU capacity available to them.

## Enforcement

On Windows, a Job Object provides native process-tree containment and CPU
limits. The real command is held behind a launch gate until its helper has been
assigned to the job, so it and the descendants it creates inherit containment.
On every platform, `with-limits` samples the resident memory of the command and
its descendants. On macOS and Linux it also samples CPU use, briefly suspending
and resuming the tracked processes to maintain the requested sustained
allowance. Wall time is supervised by `with-limits` on every platform.

The supervisor itself keeps its original priority so it can continue enforcing
limits promptly. Only the launched command and its descendants receive the
lower priority.

Sampled resident memory is the sum of resident set size (RSS) reported for the
processes in the tree. It can count shared pages more than once, so leave
headroom when processes share large mappings. Sampling also means a very brief
spike can occur between observations. Percentage and `auto` memory limits
additionally enforce the unallocated share as a host-memory reserve, so
unrelated or concurrently guarded growth can stop the command before its own
RSS reaches its ceiling.

On Unix, terminating signals `SIGHUP`, `SIGINT`, `SIGQUIT`, and `SIGTERM` sent
to `with-limits` are forwarded to the command's process group and to tracked
descendants that have created another process group or session. `with-limits`
then waits for `--kill-after` and forcibly terminates any tracked process that
remains. `SIGUSR1`, `SIGUSR2`, and `SIGWINCH` are forwarded without starting
termination. `SIGTSTP` stops the command tree and supervisor; `SIGCONT` resumes
and is forwarded to the tree. Unix limit violations request graceful
termination and use the same grace period. A Windows Job Object terminates the
tree as a unit. If required monitoring or enforcement fails, `with-limits`
stops the workload and exits with status 125.

## Exit status

The command's exit status is preserved unless the supervisor itself determines
the result:

| Status | Meaning |
| ---: | --- |
| `124` | wall-clock limit expired |
| `125` | `with-limits` could not supervise the command |
| `126` | command was found but could not be invoked |
| `127` | command was not found |
| `137` | memory limit was exceeded and the process tree was terminated |

These follow GNU `timeout` conventions where they overlap.

## Related tools

- [GNU `timeout`](https://www.gnu.org/software/coreutils/manual/html_node/timeout-invocation.html)
  (`gtimeout` under Homebrew) is mature and preferable for a time limit alone.
  `with-limits --time` follows its duration suffixes and status `124`, while
  adding portable process-tree memory and sustained-CPU controls.
- Shell `ulimit` and Linux [`prlimit`](https://man7.org/linux/man-pages/man1/prlimit.1.html)
  set kernel resource limits. Their address-space and CPU-time limits are not
  the same as aggregate resident memory and sustained CPU rate for a process
  tree.
- GNU [`nice`](https://www.gnu.org/software/coreutils/manual/html_node/nice-invocation.html)
  changes scheduling priority on Unix. `with-limits` applies the same basic
  policy automatically to the whole contained tree, adds a Windows equivalent,
  and also enforces resource ceilings.
- Linux `taskset` restricts CPU affinity but permits full use of the selected
  cores. [`cpulimit`](https://github.com/opsengine/cpulimit) uses sampled
  stop/resume control similar to the Unix CPU strategy here, but is not a GNU
  utility and focuses on one resource.
- Linux `systemd-run` can create a transient cgroup with strong native controls.
  It is a good Linux-specific choice when systemd and suitable delegation are
  available.
- [`with-gpu`](https://github.com/osteele/with-gpu) selects and monitors a GPU
  for a command. It can be used alongside `with-limits` when a workload needs
  both GPU selection and host resource containment.

## Questions and contributions

Report bugs and ask usage questions in
[GitHub Issues](https://github.com/osteele/with-limits/issues). Changes can be
proposed with a pull request.

## License

`with-limits` is released under the [MIT License](LICENSE).
