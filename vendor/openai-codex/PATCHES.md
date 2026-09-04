# Downstream patch record

OpenAI Codex source files are not modified. The vendored tree is based on
upstream commit `9474e5cfc4494b0ba319352aa86ce436c59e65c8`, with the following
auditable downstream manifest change.

## `gix` security update

`codex-rs/Cargo.toml` raises the workspace `gix` requirement from `0.81.0` to
`0.83.0`. This resolves the patched `gix`, `gix-fs`, `gix-pack`, and
`gix-packetline` components in the root `Cargo.lock`, addressing published
path-traversal, credential-disclosure, command-execution, worktree-escape, and
denial-of-service advisories. No Codex Rust source is changed.

The update passed formatting, clippy with warnings denied, all 50 first-party
tests, the signed release-tooling suite, CodeQL, and an Apple-silicon release
build with gateway self-test.

Local crates depend on selected upstream packages by path, so builds do not
silently substitute a moving Codex checkout.

The root Cargo manifest carries the two dependency overrides present in the
pinned Codex `rust-v0.133.0` workspace:

- `tokio-tungstenite` at `132f5b39c862e3a970f731d709608b3e6276d5f6`
- `tungstenite` at `9200079d3b54a1ff51072e24d81fd354f085156f`

Muse compatibility behavior lives entirely in the local `codex-transport`,
`muse-codex-gateway`, and `muse-codex` crates. Updating an upstream revision or
downstream dependency constraint requires updating this record, regenerating
`Cargo.lock`, and rerunning all applicable baseline, protocol, auth, security,
and real-account smoke tests.
