# Changelog

All notable project changes will be documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases will use
[Semantic Versioning](https://semver.org/) once a public version is published.

## [Unreleased]

### Added

- Initial Rust launcher, loopback compatibility gateway, and transport layer.
- Isolated ChatGPT and OpenAI API-key authentication flows.
- Deterministic model, streaming, error, cancellation, and launcher fixtures.
- Authenticated catalog fixtures for GPT-6 Astra and GPT-5.6 Sol, Terra, and
  Luna, without granting or fabricating model entitlements.
- Ultra reasoning selection through `--reasoning-effort ultra` and the TUI
  `/effort` picker for catalog-supported models.
- Launch-scoped `--fast` mode for TUI, `exec`, `resume`, and `serve`, with
  catalog-gated priority routing that remains independent of reasoning effort.
- Gateway readiness contract v3 so launchers cannot pair Fast mode with an
  older gateway that lacks the private selector.
- Signed private-release manifest and installer tooling.
- Project branding, community standards, support policy, and GitHub templates.
- First-class Z.ai GLM Coding Plan provider: `--provider zai`,
  `muse-codex auth set --provider zai --api-key-stdin`, and an isolated Z.ai
  keyring record that cannot satisfy a Codex launch or be removed by a Codex
  logout.
- Responses-to-chat-completions request translation and Responses event-stream
  synthesis for Z.ai, including namespaced tool-name encoding, parallel tool
  calls, reasoning summaries, and length truncation.
- A pinned GLM model catalog validated at startup against Z.ai's plan-usage
  endpoint, which consumes no coding prompt; skip it with
  `MUSE_CODEX_ZAI_SKIP_PROBE=1`.

### Changed

- Select the upstream with a `Backend` enum shared by the launcher, gateway, and
  readiness contract; the gateway self-test now advertises the providers it
  supports so a launcher cannot pair with a gateway that would ignore
  `--provider`.
- Give each provider its own isolated stock-Muse profile, because Muse's
  normalized catalog cache is keyed only by its internal provider name.
- Make sanitized failure messages and provider help text provider-neutral.
- Repin the vendored OpenAI Codex client from `rust-v0.133.0` to
  `rust-v0.153.4` (`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`) and move source builds to
  Rust `1.95.0`.
- Use the authenticated catalog's minimum-client version, visibility,
  modalities, reasoning efforts, tool mode, and Responses Lite flag to decide
  which models are offered and how requests are encoded.

### Fixed

- Dispatch `exec`, `resume`, and MSP `serve` in the correct stock CLI scope,
  preventing accidental TUI startup and terminal escapes in protocol output.
- Route authentication status, help, and invalid arguments through the Codex
  auth controller; let local harness commands work without model discovery.
- Keep stdin API keys inside the wrapper/gateway boundary and bound inherited
  API-key input before opening a child pipe.
- Preserve literal option values and avoid overwriting a running session's
  endpoint when displaying help or exporting schemas.
- Report gateway startup failures through a private, static error-code
  handshake with actionable login, Keychain, endpoint, and network guidance.
- Normalize complete Muse history into stateless Codex Responses requests,
  including message content, reasoning, parallel tool calls, and results.
- Encode the Responses Lite request contract used by current Codex models while
  keeping Muse in control of history, tools, approvals, and follow-up turns.
- Restore namespaced tool-call fields when replaying Muse's flattened history,
  fixing rejection of the request immediately after a successful tool call.
- Translate MSP provider fields without modifying opaque payloads or the
  stable schema; preserve correlated requests and reject duplicate IDs.
- Fail fast for the pinned host's broken `serve --no-session-log` mode.
- Bound the provider retry budget and turn final HTTP/SSE failures into
  sanitized terminal events; keep valid SSE usable despite incorrect MIME.
- Cancel upstream streams on disconnect and terminate orphan gateways even
  during authentication or startup.
- Preserve authenticated multi-model catalogs in Muse 1.0.3's normalized
  cache, including picker visibility, context limits, and reasoning efforts.
- Keep unknown release dates and output limits unknown instead of allowing the
  stock raw-catalog decoder to discard otherwise valid models.
- Allow slow authenticated catalog discovery to finish within a bounded
  startup handshake and provide actionable authentication guidance on failure.
- Treat `response.cancelled` as terminal, drop known
  `codex.response.metadata`, and continue to reject unknown non-metadata events.
- Open Muse's isolated Ultra feature gate so a requested Ultra session no
  longer silently falls back to ordinary `xhigh` mode.

### Security

- Per-invocation loopback bearer tokens and provider credential scrubbing.
- Size, digest, signature, platform, and filename checks for release artifacts.
- Security-fixed OpenSSL, Quinn, serde_with, gix, and rand dependency families.
- Canonical containment checks for the credential-refresh lock path.

[Unreleased]: https://github.com/Srimi1/muse-codex/commits/main
