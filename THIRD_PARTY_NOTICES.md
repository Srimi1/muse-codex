# Third-party notices

## OpenAI Codex

`codex-transport` builds against selected crates in the vendored OpenAI Codex
source at immutable Git revision
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a` (tag `rust-v0.153.4`). OpenAI Codex
is Copyright OpenAI and contributors and is licensed under the Apache License,
Version 2.0. The upstream source, license, vendor record, and documented
manifest-only security patch are included under `vendor/openai-codex`.

The proprietary Meta Muse executable is neither source nor a binary dependency
of this repository. It is discovered on the user's machine at runtime and is
not redistributed here.

Rust dependencies and their exact resolved versions/sources are recorded in
`Cargo.lock`. Their respective licenses continue to apply.
