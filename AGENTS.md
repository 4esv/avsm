# AGENTS.md

avsmd, the obedient paperclip maximizer: an agentic daemon that pursues the goals you give it, acts only within the limits you set, and has proof for everything it does.

Two binaries from the crate `avsmd`: `avsmd`, the daemon (foreground, flags `-t -T -f -d` as sshd has them; it starts its warden and roles as hidden subcommands of itself), and `avsm`, the client (and the hidden `simulate`).

## Vocabulary

Use these words and no synonyms.

- mandate, record, action, ask
- measured, declared, inferred
- report, proposal, action, ask, decision, intent, outcome, refusal, prediction, branch, forecast
- running, asking, saturated, ineffective, guarded, implausible, paused (the states, computed from records only)
- ledger, gate, proof, adapter, canary, warden, guard, audit, world, plan
- parent, child, payload, adapter (built-in metric or lever), digest, know, expects
- unwind (the one lever and payload a guard declares, run when it trips)
- escalate, escalation (the `escalate:` backend asked when the first cannot answer)
- packet (premises, options, reason: what an ask carries for the human, built by code)
- canary is the self venture's check that the gate still refuses what it must. A canary ask is a different thing: an ask that can never run, written every n asks (`canary_asks`) to measure whether the human reads what they approve. Say "canary ask", never "canary" alone, for it.
- Commands: avsmd (the daemon), add, status, tail, asks, approve, deny, pause, resume, why, know, propose, digest, forecast, doctor

## Non-negotiables

1. No action without a mandate and a proof from measured or declared facts.
2. Inferred records never authorize; they only propose or ask.
3. Only the warden holds credentials.
4. The ledger is append-only and hash-chained.
5. Stopping is always allowed. Resuming needs a declared record.
6. Irreversible actions default to asks; if delegated, record intent then outcome.
7. The built-in `self` venture cannot edit its own mandate, merge to main, or touch keys. Hardcoded.
8. No public listener. Unix socket. No live steering; humans change mandates.
9. Content a model produced is inferred until a human approves it; the record that runs it cites the declared decision, not the proposal.
10. Parents allocate; children act. A parent never runs a child's lever.
11. Anything that reads the open world is inferred by construction, schema-validated, size-bounded, and stripped of anything that is not a field before it becomes a record.
12. Self-modification is a proposal. Only a declared decision adopts one.
13. The only listener is the Tailscale one, off by default, bound to the tailnet interface only, identity from tailscaled, never from the client.
14. World models, rollouts and forecasts are inferred. They rank, veto and ask, and never authorize.
15. Simulation never runs a venture's programs: no lever, report, guard, audit or reading runs for real. In simulate and forecast every lever goes to a world model or the simulation's world, and a simulation's world is never a venture's world model.
16. Every side effect is recorded, then done: an intent with an idempotency key before the run, an outcome after. An intent with no outcome is never run again blindly.
17. The improving loop never edits limits. It may only propose delegating a reversible lever; caps, costs, guards, budgets and everything else change only by a human.

## Warden

The warden extracts text from backend replies and never interprets it. It
hands the text to the role or reads the fields the record schema names;
what the text means is never the warden's to decide.

`run` and script levers are not sandboxed. Only adapters and payload levers
are constrained by the warden. A mandate's `network: none` puts `run` in a
network namespace of its own where Linux allows one; a list of hosts holds
adapters only.

## Tests

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

## Releases

Bump `version` in Cargo.toml, update CHANGELOG.md, commit to main, then
`git tag vX.Y.Z && git push origin vX.Y.Z`. The workflow tests, builds static
Linux binaries for x86_64 and aarch64, attaches them to the GitHub release, and
publishes to crates.io when the `CARGO_REGISTRY_TOKEN` secret is set.

## Markdown

Markdown in this repo is limited to README.md, AGENTS.md, CONTRIBUTING.md, CHANGELOG.md.
Design decisions go in the issue or the commit message.
