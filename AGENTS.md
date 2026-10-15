# AGENTS.md

avsm, the obedient paperclip maximizer: an agentic daemon that pursues the goals you give it, acts only within the limits you set, and has proof for everything it does.

## Vocabulary

Use these words and no synonyms.

- mandate, record, action, ask
- measured, declared, inferred
- report, proposal, action, ask, decision, intent, outcome
- running, asking, paused
- ledger, gate, proof, adapter, canary, warden
- Commands: start, add, status, tail, asks, approve, deny, pause, resume, why

## Non-negotiables

1. No action without a mandate and a proof from measured or declared facts.
2. Inferred records never authorize; they only propose or ask.
3. Only the warden holds credentials.
4. The ledger is append-only and hash-chained.
5. Stopping is always allowed. Resuming needs a declared record.
6. Irreversible actions default to asks; if delegated, record intent then outcome.
7. The built-in `self` venture cannot edit its own mandate, merge to main, or touch keys. Hardcoded.
8. No public listener. Unix socket. No live steering; humans change mandates.

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
