---
status: accepted
date: 2026-09-07
---

# 0002. Admission reservations live in the supervisor, not in the callers

## Context and Problem Statement

`--check-headroom` and `--wait-for-headroom` answer whether the host has room
right now. They held no reservation, so callers admitted at the same instant
each saw the same headroom and none accounted for the others. Measured on a
16 GB workstation on 2026-09-07: two delegate runs admitted seconds apart were
both killed by the operating system's memory monitor minutes later, with swap
growing from 2 GB to 3 GB under them. No threshold would have prevented this,
because any threshold permissive enough to admit one of them admits both.

This is the tool's own motivating case — several agents each starting a job
that is reasonable alone while their combined use exhausts the host. The
containment half was already solved: a percentage or `auto` limit enforces the
unallocated share as a host reserve, so concurrent guarded trees react to each
other. Admission is the same problem one moment earlier.

Two independent callers on this machine each had their own copy of the
admission policy before it moved here, and had already diverged once.

## Decision Outcome

A supervised run publishes a reservation — its process id, its budget, and its
most recently sampled resident memory — into a shared per-user store for the
life of the command. Admission reads the store under an exclusive lock, deducts
the unrealized part of every live promise from free memory, and only then
compares against the floor. Reading, deciding, and publishing happen inside one
critical section, so two callers arriving together see each other.

The supervisor is the only component that brackets a command from launch to
exit, so it is the only one that can release a reservation at the moment the
work actually ends. It is also the single layer every caller already passes
through, including agent hooks that wrap a command the model generated.

### Consequences

- A previously stateless crate now keeps machine-local state, with the
  staleness and cleanup obligations that follow. Crash safety is a liveness
  check rather than a lease: a reservation whose process is gone is ignored and
  removed.
- Coordination reaches only processes that can see the same store. `TMPDIR` is
  per-user on macOS, so two accounts on one host do not see each other unless
  pointed at a shared directory. Solving that properly means a world-writable
  store where any account can publish a fabricated reservation.
- Only an explicit absolute `--memory` publishes a reservation. A budget from
  `auto` or a percentage is a cap on whatever is free rather than a claim on an
  amount, and reserving it would let the first caller reserve most of the host
  and serialize everything behind it. A caller that wants coordination must
  size its work.
- A reservation is a promise about growth that has not happened yet, so the
  deduction is deliberately conservative at the start of a command and decays
  as the tree grows into its budget.

## Considered Options

### Coordination in each caller

Rejected. Both callers already keep state that would have held a lock, and this
adds no persistence to a published crate. It loses on lifetime: a caller must
guess when the work it launched has finished, while the supervisor knows. It
also covers only the callers that implement it, leaving hook-wrapped commands
and any future caller invisible to the others.

### Serializing dispatches instead of reserving

Rejected. One-at-a-time is a blunt approximation that cannot let two small
commands run while a large one waits, and it still requires the same
cross-process coordination to implement.

## More Information

- **Builds on**: the host-reserve behaviour of percentage and `auto` limits,
  described in the README.
