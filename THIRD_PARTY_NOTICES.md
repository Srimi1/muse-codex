# Third-party notices

## OpenAI Codex

`codex-transport` builds against selected crates in the vendored OpenAI Codex
source at immutable Git revision
`9474e5cfc4494b0ba319352aa86ce436c59e65c8` (tag `rust-v0.133.0`). OpenAI Codex
is Copyright OpenAI and contributors and is licensed under the Apache License,
Version 2.0. The exact upstream tracked source and license are included under
`vendor/openai-codex`.

The proprietary Meta Muse executable is neither source nor a binary dependency
of this repository. It is discovered on the user's machine at runtime and is
not redistributed here.

Rust dependencies and their exact resolved versions/sources are recorded in
`Cargo.lock`. Their respective licenses continue to apply.
