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
- Signed private-release manifest and installer tooling.
- Project branding, community standards, support policy, and GitHub templates.

### Changed

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

### Security

- Per-invocation loopback bearer tokens and provider credential scrubbing.
- Size, digest, signature, platform, and filename checks for release artifacts.
- Security-fixed OpenSSL, Quinn, serde_with, gix, and rand dependency families.
- Canonical containment checks for the credential-refresh lock path.

[Unreleased]: https://github.com/Srimi1/muse-codex/commits/main
