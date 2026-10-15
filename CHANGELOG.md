# Changelog

## 0.1.0 - 2026-10-15

- Ledger: append-only, hash-chained records in SQLite; every record is measured, declared, or inferred.
- Mandates: `mandate.yaml` says what a venture is for, what number to watch, what it may do, and what it may not.
- Gate and why: an action is accepted only if a proof exists from measured and declared facts under a mandate; `avsm why` prints the proof or the refusal.
- Unix socket and CLI: start, add, status, tail, asks, approve, deny, pause, resume, why. No public listener.
- Kernel: supervises the venture's process, reads its metric on a cadence, decides once a night; the warden and the roles run as separate processes, and only the warden holds credentials.
- Built-in `self` venture: cannot edit its own mandate, merge to main, or touch keys.
- examples/counter: the smallest venture, to start from.
- Release pipeline: a tag push tests, builds static musl binaries for x86_64 and aarch64, attaches them to the GitHub release, and publishes to crates.io.
