# with-limits

`with-limits` runs a command with limits on its memory, sustained CPU use, and
wall-clock runtime. It supervises the command's process tree on macOS, Windows,
and Linux.

The motivating case is concurrent agents doing machine-learning or research
work. Several agents can each start a RAM-intensive job that looks reasonable
in isolation, while their combined memory use exhausts and crashes the host.
Putting a process-tree limit around each job contains that failure.

For a time limit alone, GNU `timeout` (installed as `gtimeout` by Homebrew) is
the established choice. `with-limits` is useful when one portable command
should also constrain memory or sustained CPU consumption.

## Installation

Install the current release from GitHub:

```sh
cargo install --git https://github.com/osteele/with-limits
```

Or build the current checkout:

```sh
cargo install --path .
```

`with-limits` is intended for trusted local and CI workloads. It is a resource
guard, not a security sandbox for hostile code.

## Usage

Place the command after `--`:

```sh
with-limits --memory 8GiB --cpu 2 --time 30m -- python train.py
```

Use `-c` to run a shell command. On macOS this uses `/bin/zsh`; on other Unix
systems it uses `/bin/sh`; and on Windows it uses `cmd.exe`.

```sh
with-limits --memory 4GiB -c 'just format && just check'
```

When no limit option is supplied, `with-limits` applies `--memory auto`, which
is 70% of the memory available when the command starts:

```sh
with-limits -c 'just format && just check'
```

Options:

- `--memory SIZE`, `-m SIZE` limits aggregate resident memory. Sizes accept SI
  suffixes such as `GB`, IEC suffixes such as `GiB`, `auto`, or a percentage of
  initially available memory such as `60%`.
- `--cpu CORES` limits sustained CPU use, where `1` is the capacity of one
  logical core. Fractional values such as `0.5` are accepted.
- `--time DURATION`, `-t DURATION` limits wall-clock runtime. Durations use the
  GNU `timeout` suffixes `s`, `m`, `h`, and `d`; milliseconds use `ms`.
- `--kill-after DURATION` (also `--grace`) controls how long graceful
  termination may take before the process tree is forcibly terminated. The
  default is two seconds.
- `--shell PATH` selects the shell used by `-c`.
- `--require-native` rejects a requested memory or CPU limit if the platform
  would enforce it by sampling.
- `--quiet`, `-q` suppresses the interactive startup summary. Limit violations
  are still reported.

## Enforcement

On Windows, a Job Object provides native process-tree memory and CPU limits and
ensures descendants are terminated with the job. On macOS and Linux,
`with-limits` samples the resident memory and CPU use of the command and its
descendants. CPU enforcement briefly suspends and resumes the command's process
group to maintain the requested sustained allowance. Wall time is supervised
by `with-limits` on every platform.

Sampled resident memory is the sum reported for the processes in the tree. It
can count shared pages more than once, so leave headroom when processes share
large mappings. Sampling also means a very brief spike can occur between
observations.

Signals sent to `with-limits` are forwarded to the Unix process group. On Unix,
a limit first requests graceful termination, waits for `--kill-after`, and then
forces termination if any tracked descendant remains. A Windows Job Object
terminates the tree as a unit.

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
  changes scheduling priority; it does not impose a CPU ceiling.
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

## License

`with-limits` is released under the [MIT License](LICENSE).
