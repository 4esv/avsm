# Changelog

## 1.0.0 - 2026-10-16

- Payloads: proposals and asks carry the text, json, diff or file a lever consumes; `takes:` on a lever; a model's content runs only after a declared approval unless delegated; `avsm asks` shows it, `avsm approve <n> --edit` amends it, `avsm propose` files one by hand.
- Adapters: built-in metrics (stripe.mrr, stripe.charges_today, github.open_issues, github.failed_ci, imap.unread, http.status, file.number, shell) and levers (github.issue, github.comment, github.pr, mail.send, http.post), run in the warden with the one key each is scoped to; a venture can be one mandate file.
- Research role: the built-in `research` lever sends a question to the command named under `research:` and keeps the answers as inferred facts with source and confidence, bounded and stripped to the record's shape.
- Parents: `children:` and `parent:`, loops refused at add; parent levers budget, pause, resume (declared-only) and threshold; shared quotas for asks, spend and sends, refused before the gate and shown by `why`; a parent's nightly call sees its children's records, not their raw input.
- Knowledge: `avsm know <venture|topic>` lists current facts by a Datalog query where superseding is a later record citing the old; summaries past `summarize_after` keep the context small and the ledger whole; a model may drill with know and why, and every drill is a record.
- Ratchet and digest: after `ratchet` approvals of one lever and payload shape, `self` proposes the mandate change that delegates it; approving rewrites the mandate and re-adopts it. `avsm digest`, written nightly, with asks and incidents a week.
- Tailscale access: `listen: tailscale`, bound to the tailnet address only, identity from tailscaled, logins under `remote_users:`; declared records carry the login; `avsm --remote host:port`.
- Hardening: `network:` per venture, held for adapters by the warden and for `run` by a network namespace where Linux allows one; ci.yml on every push to main; AGENTS.md states the warden extracts and never interprets.
- Examples: github-repo, stripe-store, mail-inbox, paper-trader, greenhouse, portfolio beside counter, each verified end to end with stubs; README for 1.0.

## 0.1.0 - 2026-10-15

- Ledger: append-only, hash-chained records in SQLite; every record is measured, declared, or inferred.
- Mandates: `mandate.yaml` says what a venture is for, what number to watch, what it may do, and what it may not.
- Gate and why: an action is accepted only if a proof exists from measured and declared facts under a mandate; `avsm why` prints the proof or the refusal.
- Unix socket and CLI: start, add, status, tail, asks, approve, deny, pause, resume, why. No public listener.
- Kernel: supervises the venture's process, reads its metric on a cadence, decides once a night; the warden and the roles run as separate processes, and only the warden holds credentials.
- Built-in `self` venture: cannot edit its own mandate, merge to main, or touch keys.
- examples/counter: the smallest venture, to start from.
- Release pipeline: a tag push tests, builds static musl binaries for x86_64 and aarch64, attaches them to the GitHub release, and publishes to crates.io.
