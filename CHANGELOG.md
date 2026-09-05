# Changelog

All notable project changes will be documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases will use
[Semantic Versioning](https://semver.org/) once a public version is published.

## [Unreleased]

### Added

- Initial Rust launcher, loopback compatibility gateway, and transport layer.
- Isolated ChatGPT and OpenAI API-key authentication flows.
- Deterministic model, streaming, error, cancellation, and launcher fixtures.
- Signed private-release manifest and installer tooling.
- Project branding, community standards, support policy, and GitHub templates.

### Fixed

- Preserve authenticated multi-model catalogs in Muse 1.0.3's normalized
  cache, including picker visibility, context limits, and reasoning efforts.
- Keep unknown release dates and output limits unknown instead of allowing the
  stock raw-catalog decoder to discard otherwise valid models.
- Allow slow authenticated catalog discovery to finish within a bounded
  startup handshake and provide actionable authentication guidance on failure.

### Security

- Per-invocation loopback bearer tokens and provider credential scrubbing.
- Size, digest, signature, platform, and filename checks for release artifacts.
- Security-fixed OpenSSL, Quinn, serde_with, gix, and rand dependency families.
- Canonical containment checks for the credential-refresh lock path.

[Unreleased]: https://github.com/Srimi1/muse-codex/commits/main
