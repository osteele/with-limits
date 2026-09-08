# Decision log

- **2026-09-09** — In the context of admission for commands with automatic or percentage memory caps, facing a cap that cannot also express a modest growth estimate, we decided to add an independent absolute `--reserve` amount and neglected requiring callers to use an absolute `--memory` limit, to let containment remain generous while concurrent starts account for one another, accepting that callers must estimate expected memory separately from the enforced cap.
