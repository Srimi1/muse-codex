# Downstream patch record

OpenAI Codex source files are not modified. The vendored tree is based on
upstream commit `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a` (`rust-v0.153.4`), with
the following auditable downstream manifest change.

## `gix` security update

`codex-rs/Cargo.toml` raises the workspace `gix` requirement from `0.81.0` to
`0.83.0`. This resolves the patched `gix`, `gix-fs`, `gix-pack`, and
`gix-packetline` components in the root `Cargo.lock`, addressing published
path-traversal, credential-disclosure, command-execution, worktree-escape, and
denial-of-service advisories. No Codex Rust source is changed.

Repin qualification must include formatting, Clippy with warnings denied, all
first-party tests, release tooling, the stock/wrapped compatibility suite, an
Apple-silicon release build with gateway self-test, and opt-in real-account
smoke tests before an artifact is released.

Local crates depend on selected upstream packages by path, so builds do not
silently substitute a moving Codex checkout.

The root Cargo manifest mirrors the three dependency overrides present in the
pinned Codex `rust-v0.153.4` workspace:

- `crossterm` at `45fecb9508105988f42fe6ff0441783ed3717f92`
- `tokio-tungstenite` at `0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186`
- `tungstenite` at `4fffad30fe373adbdcffab9545e9e9bf4f2fc19f`

Muse compatibility behavior lives entirely in the local `codex-transport`,
`muse-codex-gateway`, and `muse-codex` crates. Updating an upstream revision or
downstream dependency constraint requires updating this record, regenerating
`Cargo.lock`, and rerunning all applicable baseline, protocol, auth, security,
and real-account smoke tests.
