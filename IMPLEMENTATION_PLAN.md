# muse-codex implementation plan

## Constraint

Meta does not publish the Muse 1.0.3 host source. This repository therefore implements the provider swap at the supported runtime seam: a `muse-codex` launcher starts a private loopback gateway and runs the stock Muse binary against it. Muse continues to own its TUI, sessions, prompts, tools, approvals, sandbox, extensions, and orchestration.

## Deliverables

1. A Rust workspace containing the launcher, gateway, and shared protocol/types.
2. ChatGPT subscription and API-key authentication isolated from stock Codex and Muse state.
3. `GET /muse-code/models` and streaming `POST /responses` compatibility endpoints backed by pinned OpenAI Codex client crates.
4. CLI interception for `login`, `logout`, and `auth set --provider codex --api-key-stdin`; all other arguments pass through to Muse with the gateway injected.
5. Deterministic unit/integration fixtures for request mapping, SSE streaming, errors, cancellation, credential redaction, and launcher argument handling.
6. A macOS arm64 installer and signed release-manifest tooling that never redistributes the proprietary Muse binary.

## Release gates

- No prompts or credentials are sent to Meta model/auth/catalog endpoints.
- Credentials never appear in argv, child environments, logs, exports, or traces.
- Muse commands and provider-independent behavior remain pass-through compatible.
- Stream retries cannot duplicate tool calls or other side effects.
- Authentication failures never fall back silently between subscription and API billing.
- The project builds and its offline test suite passes on macOS arm64.

## Upstream pins

- Muse behavior baseline: `1.0.3-R2198.1`.
- OpenAI Codex client: `rust-v0.133.0`, commit `9474e5cfc4494b0ba319352aa86ce436c59e65c8`.

