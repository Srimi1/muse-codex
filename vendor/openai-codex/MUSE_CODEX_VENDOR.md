# Muse Codex vendor record

This directory is derived from the tracked files in the OpenAI Codex repository
at the exact revision used by `muse-codex`. Downstream additions and the single
manifest-only security patch are recorded in `PATCHES.md`.

- Repository: <https://github.com/openai/codex>
- Tag: `rust-v0.133.0`
- Commit: `9474e5cfc4494b0ba319352aa86ce436c59e65c8`
- Rust toolchain: `1.93.0`

The project selects upstream crates through local path dependencies rooted in
`codex-rs`. OpenAI source files remain unchanged; only
`codex-rs/Cargo.toml` differs to select a security-fixed `gix` release.

To audit the upstream baseline, compare its tracked paths and blob contents
with:

```sh
git -C /path/to/openai-codex ls-tree -r 9474e5cfc4494b0ba319352aa86ce436c59e65c8
```

Any expected difference must be named in `PATCHES.md`.
