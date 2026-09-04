# Muse Codex vendor record

This directory is an unmodified archive of the tracked files in the OpenAI
Codex repository at the exact revision used by `muse-codex`.

- Repository: <https://github.com/openai/codex>
- Tag: `rust-v0.133.0`
- Commit: `9474e5cfc4494b0ba319352aa86ce436c59e65c8`
- Rust toolchain: `1.93.0`

The project selects upstream crates through local path dependencies rooted in
`codex-rs`. `PATCHES.md` records all downstream changes; the upstream files
themselves are not patched.

To audit this directory, compare its tracked paths and blob contents with:

```sh
git -C /path/to/openai-codex ls-tree -r 9474e5cfc4494b0ba319352aa86ce436c59e65c8
```
