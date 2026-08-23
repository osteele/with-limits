---
status: accepted
date: 2026-08-23
---

# 0001. Use a fixed lower priority for the child tree

## Context and Problem Statement

`with-limits` exists to keep a developer's machine responsive while agents and
other background workloads perform CPU- and memory-intensive work. Resource
ceilings contain runaway jobs, but a job below its CPU ceiling can still
compete equally with an editor, browser, terminal, or other interactive
application.

Scheduling policy could respond dynamically to system load or the number of
agents. That would require defining a load signal, sampling it, choosing stable
thresholds across operating systems, and avoiding oscillation. The operating
system scheduler already has the information needed to prefer foreground work
while giving spare CPU capacity to background work.

## Decision Outcome

Run the entire launched process tree at a fixed lower scheduling priority. On
Unix, increase the child's inherited niceness by 10 by default. On Windows,
apply the Below Normal priority class to the Job Object. Descendants inherit or
remain constrained by the corresponding platform mechanism.

Keep the `with-limits` supervisor at its original priority so monitoring,
signal forwarding, and enforcement remain prompt. `WITH_LIMITS_NICE` may set a
different positive Unix adjustment or disable priority lowering. On Windows,
any enabled value selects Below Normal priority.

Do not vary priority according to load, agent count, observed CPU use, or other
runtime signals. CPU-rate enforcement remains a separate mechanism.

### Consequences

- Foreground applications receive preferential CPU scheduling during
  contention, while guarded jobs may consume otherwise-idle cores.
- Behavior is predictable and does not require global coordination among
  agents.
- Priority is a preference, not a CPU, memory, or I/O ceiling; the existing
  limits remain necessary.
- Unix exposes a numeric adjustment while Windows exposes only the coarser
  Below Normal mapping through this configuration.
- Nested guarded commands may inherit an already-lowered priority and lower it
  further, up to the Unix niceness ceiling.

## Considered Options

### Dynamically adjust priority

Rejected: load- and agent-dependent control duplicates scheduler policy, adds
platform-specific tuning and feedback behavior, and makes command performance
less predictable.

### Leave scheduling priority unchanged

Rejected: resource ceilings alone do not ensure that interactive work wins CPU
contention below those ceilings.

### Lower the supervisor together with the child tree

Rejected: enforcement and signal forwarding should not be delayed behind the
workload they supervise.

## More Information

- **References**: [POSIX `exec` process-attribute inheritance](https://pubs.opengroup.org/onlinepubs/7908799/xsh/exec.html), [Windows Job Object priority-class limits](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information)
