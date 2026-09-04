# Downstream patch record

No OpenAI Codex source file is modified by this repository.

The complete tracked contents of upstream commit
`9474e5cfc4494b0ba319352aa86ce436c59e65c8` are vendored in this directory.
Local crates depend on the selected upstream packages by path, so builds do not
silently substitute a moving Codex checkout.

The root Cargo manifest carries the two dependency overrides present in the
pinned Codex `rust-v0.133.0` workspace:

- `tokio-tungstenite` at `132f5b39c862e3a970f731d709608b3e6276d5f6`
- `tungstenite` at `9200079d3b54a1ff51072e24d81fd354f085156f`

Muse compatibility behavior lives entirely in the local `codex-transport`,
`muse-codex-gateway`, and `muse-codex` crates. Updating any upstream revision
requires updating this record, regenerating `Cargo.lock`, and rerunning all
baseline, protocol, auth, security, and real-account smoke tests.
