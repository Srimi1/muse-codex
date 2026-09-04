# Contributing to Muse Codex

Thank you for helping improve Muse Codex. Contributions are welcome when they
preserve the project's central boundary: the stock Muse executable owns the
agent harness, while this repository owns only authentication, process wiring,
and the model-transport gateway.

By participating, you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before opening a change

- Search existing issues and the [roadmap](ROADMAP.md).
- Use a feature request for changes that affect architecture, compatibility,
  authentication, or release policy.
- Report suspected vulnerabilities through the private route in the
  [security policy](.github/SECURITY.md), never in a public issue.
- Do not include a Muse binary, proprietary Muse files, credentials, prompts,
  private-feed URLs, or signing material.

## Development setup

Development requires Apple-silicon macOS, Rust `1.93.0`, and the exact stock
Muse Code baseline `1.0.3-R2198.1` for host integration checks. Unit and fixture
tests do not require OpenAI credentials.

```sh
git clone https://github.com/Srimi1/muse-codex.git
cd muse-codex
rustup show
cargo build --workspace --locked
```

See [installation](docs/INSTALLATION.md) and
[configuration](docs/CONFIGURATION.md) for the complete local setup.

## Required checks

Run these before submitting a pull request:

```sh
cargo fmt -p codex-transport -p muse-codex -p muse-codex-gateway -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
bash tests/scripts/release-tooling-test.sh
```

The formatting command is intentionally scoped to first-party crates; the
vendored Codex tree preserves its upstream formatting.

## Change guidelines

- Keep changes small enough to review and include tests for behavior changes.
- Preserve exact tool names, call IDs, arguments, event order, and terminal
  states across transport translation.
- Never retry after an output item or tool call may have become observable.
- Keep ChatGPT subscription auth and API-key auth explicit and separate.
- Fail closed when a version, credential, endpoint, or release artifact cannot
  be verified.
- Update documentation and `CHANGELOG.md` for user-visible changes.
- Do not weaken secret scrubbing, loopback-only binding, file permissions, or
  signed-release checks to make a test pass.

## Vendored source

`vendor/openai-codex` is an immutable snapshot of the upstream OpenAI Codex
repository. Avoid editing it directly. A vendor update must:

1. name an exact upstream commit and tag;
2. update `vendor/openai-codex/MUSE_CODEX_VENDOR.md` and `PATCHES.md`;
3. preserve upstream license and notice files;
4. refresh lockfiles deliberately; and
5. pass the full auth, transport, error, cancellation, and compatibility suite.

## Pull requests

Explain the problem, design, validation, and security or compatibility impact.
CI must pass. A maintainer may ask for a smaller change or additional fixture
coverage when a patch crosses the host/provider boundary.

Unless stated otherwise, submitted contributions are licensed under the
[Apache License 2.0](LICENSE).
