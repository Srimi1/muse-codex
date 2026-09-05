# Muse Codex vendor record

This directory is derived from the tracked files in the OpenAI Codex repository
at the exact revision used by `muse-codex`. Downstream additions and the single
manifest-only security patch are recorded in `PATCHES.md`.

- Repository: <https://github.com/openai/codex>
- Tag: `rust-v0.153.4`
- Commit: `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`
- Rust toolchain: `1.95.0`

The project selects upstream crates through local path dependencies rooted in
`codex-rs`. OpenAI source files remain unchanged; only
`codex-rs/Cargo.toml` differs to select a security-fixed `gix` release.

To audit the upstream baseline, compare its tracked paths and blob contents
with:

```sh
git -C /path/to/openai-codex ls-tree -r 3d2ee51ca2d5db578f328aa75e20aa22c0197c9a
```

Any expected difference must be named in `PATCHES.md`.
