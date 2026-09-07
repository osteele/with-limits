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

A supervised run publishes a reservation — its process identity, its budget,
and its most recently sampled resident memory — into a per-account store for
the life of the command. Identity is the process id together with that
process's start time, so an id the operating system later reuses does not
inherit the record. Admission reads the store under an exclusive lock, deducts
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
  check rather than a lease: a supervisor killed without running its cleanup
  leaves a record, and the next scan reaps it once the process is gone or its
  id has been reused by a process with a different start time. Where a
  platform will not report a start time, identity falls back to the id alone
  and a reused id is indistinguishable from the original — the one case the
  check cannot cover.
- **Coordination is per-account, and cross-account sharing is not available.**
  On macOS `TMPDIR` is not merely per-user but mode 700, so two accounts on one
  host cannot see each other's store even when both are willing: measured on
  this workstation and on studio, where `agent` and the primary account each
  hold a private store and neither can read the other's. Pointing both at one
  configured directory does not rescue it either: the lock file and the records
  are created 0600 by design, so a second account fails rather than shares. A
  world-writable store would additionally let any account publish a fabricated
  reservation, delete another's, or read what it is running. Treat reservations
  as coordinating the processes of one account, and nothing wider.
- Only an explicit absolute `--memory` publishes a reservation. A budget from
  `auto` or a percentage is a cap on whatever is free rather than a claim on an
  amount, and reserving it would let the first caller reserve most of the host
  and serialize everything behind it. A caller that wants coordination must
  size its work.
- A reservation is a promise about growth that has not happened yet, so the
  deduction is deliberately conservative at the start of a command and decays
  as the tree grows into its budget.
- **A store failure must not propagate past the process that hit it.** Two
  paths were made to fail softly for this reason. Refreshing a live
  reservation warns once and drops the reservation rather than propagating,
  because the alternative killed the supervised command: the update runs on
  the poll tick and ahead of graceful termination, so an error there converted
  a documented memory-limit stop into an ungraceful kill. And a read takes the
  lock only if it can, because a consumer that asks whether there is headroom
  and receives an error treats it as "unknown" and drops its own memory gate
  quietly, which is the opposite of what a gate is for. Publishing the initial
  reservation stays fatal, since failing before the command starts is safe.

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
