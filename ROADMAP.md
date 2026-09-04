# Roadmap

Muse Codex is experimental and has not published a stable binary release. This
roadmap separates implemented source functionality from the evidence required
before a broader release.

## Available on `main`

- Rust launcher, private loopback gateway, and Responses stream translation.
- Browser, device, and API-key authentication with isolated credential state.
- Exact Muse Code `1.0.3-R2198.1` version gate.
- Unit and deterministic fixture coverage for first-party crates.
- Signed private-release manifest and installer tooling.
- Architecture, security, installation, configuration, and contribution docs.

## Before the first supported release

- Complete recorded stock-vs-wrapped compatibility transcripts for supported
  commands and approval paths.
- Add opt-in live OpenAI validation without exposing credentials to CI logs.
- Exercise cancellation and ambiguous-disconnect behavior end to end.
- Produce SBOM, build provenance, checksums, license notices, and signed release
  artifacts from protected release infrastructure.
- Complete an external security and privacy review.
- Decide whether binary distribution remains private or becomes public.

## Later

- Qualify newer Muse Code builds one at a time.
- Add auxiliary provider-backed routes only with explicit fixtures.
- Expand supported platforms only after equivalent keyring, process, and
  sandbox behavior is proven.

Roadmap items are intentions, not commitments. Open a feature request before
starting work that changes the host/provider boundary.
