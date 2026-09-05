# Pinned dependency advisory review

Review date: 2026-09-05

This repository intentionally vendors OpenAI Codex `rust-v0.133.0` at revision
`9474e5cfc4494b0ba319352aa86ce436c59e65c8`. The five Dependabot alerts below
remain open and version-flagged. This review records whether each vulnerable
operation is reachable through the current `muse-codex-gateway` helper; it is
not a blanket assertion that the binaries or dependencies are free of security
defects.

## Findings

| Dependency | Advisory and affected version | Activation condition | Current helper path |
| --- | --- | --- | --- |
| `jsonwebtoken 9.3.1` | [GHSA-h395-gr6q-cpjc](https://github.com/advisories/GHSA-h395-gr6q-cpjc), fixed in `10.3.0` | Validating a malformed optional `exp` or `nbf` claim when its validation flag is enabled but the claim is not required | The only production use is agent-identity JWT verification in `vendor/openai-codex/codex-rs/agent-identity/src/lib.rs`. That claim type requires numeric `exp`, `Validation::new` retains required expiration validation, and `nbf` validation is not enabled. The launcher removes `CODEX_ACCESS_TOKEN` from the helper environment and `Transport::resolve_auth` rejects agent-identity credentials. Browser subscription and API-key authentication do not call this decoder. |
| `rmcp 0.15.0` | [GHSA-89vp-x53w-74fx](https://github.com/advisories/GHSA-89vp-x53w-74fx), fixed in `1.4.0` | Serving MCP over rmcp's Streamable HTTP server transport without `Host` validation | The dependency features in `vendor/openai-codex/codex-rs/app-server-protocol/Cargo.toml` enable `base64`, `macros`, `schemars`, and `server`; the resolved graph enables `transport-async-rw`, not `transport-streamable-http-server` or `server-side-http`. The helper's loopback gateway is its own HTTP implementation and does not instantiate an rmcp HTTP server. |
| `opentelemetry_sdk 0.31.0` | [GHSA-w9wp-h8wv-79jx](https://github.com/advisories/GHSA-w9wp-h8wv-79jx), fixed in `0.32.1` | Calling `BaggagePropagator::extract_with_context` on an oversized attacker-controlled inbound `baggage` header | There are no `BaggagePropagator` calls in the repository. The pinned paths in `vendor/openai-codex/codex-rs/codex-client/src/default_client.rs` and `vendor/openai-codex/codex-rs/otel/src/trace_context.rs` use `TraceContextPropagator`; the helper does not initialize `codex-otel`. |
| `hickory-proto 0.25.2` | [GHSA-3v94-mw7p-v465](https://github.com/advisories/GHSA-3v94-mw7p-v465); affected range through `0.25.2` | DNSSEC NSEC3 validation with `dnssec-ring` or `dnssec-aws-lc-rs`, followed by a cross-zone response with a mismatched SOA owner | The resolved helper feature graph enables `futures-io`, `std`, and `tokio`, but no DNSSEC feature. Hickory is present only through `rama-dns` in `codex-network-proxy`. |
| `hickory-proto 0.25.2` | [GHSA-q2qq-hmj6-3wpp](https://github.com/advisories/GHSA-q2qq-hmj6-3wpp), fixed in `0.26.1` | Encoding a malicious DNS message with enough compression candidates to trigger quadratic work | The helper constructs a direct `reqwest::Client` in `crates/codex-transport/src/transport.rs`; it never builds or runs `codex_network_proxy::NetworkProxy`. The Rama/Hickory implementation is linked through pinned shared protocol/config types but is not on the helper's request, login, model-catalog, or stream path. |

## Evidence commands

Run from the repository root:

```sh
cargo tree -p codex-transport --locked --offline -i jsonwebtoken@9.3.1
cargo tree -p codex-transport --locked --offline -i rmcp@0.15.0
cargo tree -p codex-transport --locked --offline -i opentelemetry_sdk@0.31.0
cargo tree -p codex-transport --locked --offline -i hickory-proto@0.25.2
cargo tree -p codex-transport --locked --offline -e features -i rmcp@0.15.0
cargo tree -p codex-transport --locked --offline -e features -i hickory-proto@0.25.2
rg 'BaggagePropagator|extract_with_context' crates vendor/openai-codex/codex-rs
rg 'NetworkProxy::builder|NetworkProxyBuilder' crates/codex-transport crates/muse-codex-gateway
```

The absence checks are intentionally scoped to the shipped helper and gateway.
Stock or future Codex crates elsewhere in the vendored workspace may activate
different features or call paths.

## Upgrade decision

None of these is a safe lockfile-only update under the required Codex revision:

- `jsonwebtoken 10.3` and `rmcp 1.4` are major upgrades that require source and
  feature migration across the pinned crates.
- `opentelemetry_sdk 0.32.1` requires a coordinated upgrade of the matching
  OpenTelemetry, OTLP, semantic-conventions, appender, and tracing integration
  crates.
- `rama-dns 0.3.0-alpha.4` constrains `hickory-resolver` to the `0.25` line, so
  Hickory `0.26.1` requires a Rama upgrade or a separately maintained backport.

The alerts should be resolved by a reviewed Codex vendor-revision update or by
an explicit, tested backport—not by silently repinning incompatible transitive
dependencies.

## Re-review triggers

Repeat this review before release if any of the following happens:

- the pinned Codex revision or any of the four dependency families changes;
- agent-identity authentication is accepted or `CODEX_ACCESS_TOKEN` is passed
  to the helper;
- rmcp Streamable HTTP server or `server-side-http` features are enabled;
- an OpenTelemetry baggage propagator is configured or inbound `baggage`
  headers are extracted;
- `codex-network-proxy` is constructed by the helper or gateway;
- Hickory DNSSEC features are enabled; or
- the helper starts accepting or encoding untrusted DNS messages.
