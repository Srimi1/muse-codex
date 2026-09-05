# Security model

## Status and scope

This document separates controls enforced by the current code from properties
that remain release requirements. "Enforced" means the repository contains an
explicit implementation; focused automated coverage is identified separately.
"Release requirement" means the property is required for a supported release
but still needs end-to-end or operational verification.

Current CI exercises Rust unit and fixture tests plus the release-tooling shell
suite. Stock-versus-wrapped behavior and live OpenAI behavior are planned
release gates; a green CI run does not yet prove those system-level properties.

The local operating-system account is trusted to read its own processes and
files. Root, debuggers, injected dynamic libraries, a compromised keyring, and a
fully compromised user account are outside this threat model.

## Security contract

### Enforced controls

- The launcher starts the gateway on `127.0.0.1` with an ephemeral port, and the
  gateway rejects a non-loopback bind.
- Every gateway route requires a per-process, 256-bit bearer token. Stock Muse
  receives that loopback token rather than an OpenAI credential.
- The launcher replaces Muse's provider endpoint with the loopback gateway and
  removes inherited provider credentials and routing variables from both child
  processes. The wrapper has no fallback route to a Meta model endpoint.
- ChatGPT subscription credentials and API keys remain distinct modes. A custom
  upstream base URL is accepted only with explicit API-key authentication.
- The installer verifies the signed manifest and each executable's platform,
  filename, byte length, and SHA-256 digest before installation.

### Release requirements

- The provider swap must preserve stock Muse's approval, sandbox, workspace,
  session, and extension behavior.
- Retry and cancellation behavior must not cause Muse to execute an observable
  tool call more than once.
- Credentials and private-feed secrets must remain absent from diagnostics,
  traces, session exports, crash reports, and future telemetry.
- The stock-versus-wrapped fixture lanes and live OpenAI lane described in
  [Architecture](ARCHITECTURE.md#compatibility-strategy) must pass before a
  release is described as compatibility-verified.

## Trust boundaries

```text
untrusted workspace content
          |
          v
stock Muse approval/sandbox boundary
          |
          v
stock Muse process -- ephemeral bearer --> loopback gateway -- OpenAI auth --> OpenAI

private feed -- signed manifest --> installer -- verified wrapper --> install directory
                    ^
                    |
             independently supplied public key
```

The gateway is inside the local user's trust boundary but outside Muse's tool
execution boundary. It authenticates provider traffic and validates transport
framing; it does not approve or execute model-requested tools.

## Protected assets

- ChatGPT/Codex authorization tokens and refresh material
- OpenAI API keys
- private-feed credentials and signed release URLs
- prompts, repository content, tool outputs, images, and session history
- the integrity of tool-call IDs and arguments
- Muse approval decisions and sandbox configuration
- the release-signing private key

## Credential handling

### Enforced controls

- Persistent OpenAI credentials use the upstream Codex keyring implementation
  with `Keyring` mode; this project does not persist them to `auth.json`.
- The keyring namespace is derived from a canonical, isolated Muse Codex auth
  home. It defaults to the platform data directory under
  `muse-codex/codex-home`; `MUSE_CODEX_HOME` may override it subject to path,
  ownership, and permission checks.
- The launcher removes `CODEX_ACCESS_TOKEN`, `CODEX_API_KEY`, `CODEX_HOME`,
  `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, `META_API_KEY`, `OPENAI_API_KEY`,
  `OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`, `OPENAI_PROJECT`, custom Muse
  headers, and the configured OTLP endpoint from child environments.
- An invocation-only `OPENAI_API_KEY` is copied into a bounded, zeroed-on-drop
  buffer, delivered to the gateway through stdin, and omitted from child argv.
  This does not claim that the operating system's original process-environment
  storage can be securely erased.
- Stored API keys are read from bounded stdin, trimmed, checked for emptiness and
  whitespace, and never accepted as a command-line value.
- Custom upstream base URLs require API-key mode, HTTPS, and no embedded user
  information, query, or fragment. The HTTP client does not follow redirects.
- Logout targets only the keyring record associated with the Muse Codex auth
  home.

Browser login uses the upstream Codex localhost callback flow. Device login
prints the verification URL and user code. Those values are intentionally
user-visible while the flow is active.

### Operational requirements

- Treat browser authorization URLs, device codes, API keys, and signed release
  URLs as secrets while valid; do not paste them into issues or persistent logs.
- If logging or telemetry is added, use field-level allowlists and tests that
  prove authorization headers and token-shaped values are redacted.
- Protect the operating-system account and keyring. The wrapper cannot defend
  credentials against another process with equivalent user privileges.

### Muse child credential

Each invocation receives a cryptographically random bearer token scoped to its
gateway process. The token is written to a mode-`0600` readiness file in a
private temporary directory, validated by the launcher, and supplied to stock
Muse as the local endpoint credential. Gateway middleware compares it exactly
and in constant time before dispatching a route. The launcher terminates the
gateway and removes its temporary readiness directory during teardown.

### Private-feed credentials

The installer accepts exact manifest and signature URLs plus a caller-supplied
public-key file; it does not download its trust anchor. Optional feed
authentication belongs in a regular, non-symlink, mode-`0600` curl config file
selected by `MUSE_CODEX_RELEASE_CURL_CONFIG`. Download URLs are supplied to curl
through standard input rather than as process arguments.

The release-signing private key is an operational secret. It must remain in
protected release infrastructure and is never an installer input.

## Network controls

### Enforced controls

- Normal launches bind exactly `127.0.0.1:0`; the gateway independently rejects
  any non-loopback listener address.
- All gateway routes, including health and catalog discovery, pass through
  bearer authentication.
- Default upstream routing comes from the pinned Codex provider implementation.
  A user-selected API-key endpoint may target any URL that passes the strict
  HTTPS base-URL validation; there is no local upstream-host or model allowlist.
- The upstream HTTP client uses certificate verification and disables redirects.
- Inbound request bodies are limited to 64 MiB. Upstream model catalogs are
  limited to 8 MiB, private gateway readiness documents to 10 MiB, captured
  error bodies to 64 KiB, individual SSE events to 8 MiB, and SSE idle periods
  to 120 seconds.
- Only `accept-language` and `x-request-id` are copied from Muse to upstream
  transport input. Authorization, cookie, proxy, host, connection, originator,
  account, and user-agent headers are not forwarded.
- Downstream response headers are allowlisted to content type, request ID,
  retry metadata, and OpenAI/Codex rate-limit metadata; hop-by-hop headers are
  not copied.
- Search and browser-open requests have explicit Codex-compatible gateway
  routes. Unknown routes fail locally; they do not fall back to Meta.

### Release requirements and known limits

- The application does not currently set an explicit HTTP header-size limit;
  dependency defaults apply. A supported release should define and test that
  limit rather than imply one is already enforced.
- Proxy and custom-CA behavior comes from the HTTP stack and process
  environment. Operators must treat either as trusted configuration, and the
  release suite must verify the intended deployment policy.
- End-to-end traffic capture must confirm that wrapped model, authentication,
  and catalog requests use only the loopback gateway and selected OpenAI
  endpoint. Unit tests alone cannot establish that property for an opaque Muse
  executable.

## Tool-call integrity, retries, and cancellation

### Current behavior

The gateway never executes tools. It forwards the Muse Responses JSON after
forcing `store: false` and streaming mode. For successful Responses streams, it
forwards known non-metadata SSE frames without rewriting response IDs, item IDs,
call IDs, tool names, JSON arguments, or sequence numbers. It does not attempt
to repair malformed JSON. Malformed, unknown, oversized, interrupted, or idle
streams become a terminal `response.failed` event.

The transport permits at most two attempts for retryable send failures or
upstream 5xx responses before a response body is exposed, and supports the
pinned Codex 401 credential-refresh flow. It performs no transport retry after
the response body is returned to Muse. This prevents a locally observed tool
call from being replayed by the gateway, but it does not claim that upstream
compute was never attempted twice.

Dropping the downstream stream tears down the associated upstream response
stream. It does not assert that OpenAI forgot prompt data already received or
that an upstream computation produced no side effects.

### Release requirements

- End-to-end fixture gates must prove byte-for-byte tool name, argument,
  call-ID, item-ID, ordering, and terminal-state preservation across fragmented
  streams.
- Stock-versus-wrapped tests must prove that approval prompts and sandbox
  decisions occur exactly once for each observable tool call.
- Live tests must exercise cancellation before output, during text output, and
  during fragmented function arguments without an automatic replay.

## Stock Muse safety boundary

Stock Muse remains the owner of tool schemas, approvals, execution, sandboxing,
workspace trust, and sessions. The wrapper does not contain a tool executor and
does not intentionally alter those controls. Equivalence is nevertheless a
release requirement until the planned black-box stock-versus-wrapped tests cover
command approval, writes outside the workspace, network access, resume, and
export behavior.

Muse hooks and MCP servers retain their native behavior. They may execute
outside portions of Muse's command sandbox and may access the network. A trusted
provider does not make an untrusted hook, MCP server, skill, plugin, or
repository safe. Keep workspace trust enabled and review extensions before use.

## Release and installer security

### Enforced controls

The release manifest is signed with an Ed25519 SSHSIG key through the platform
`ssh-keygen` implementation. It identifies the schema, product, release and
minimum-tested Muse versions, publication timestamp, and each required
`macos_arm64` executable's fixed filename, HTTPS URL, byte length, and SHA-256
digest, together with equivalent metadata for the legal payloads.

`scripts/install.sh` verifies the detached manifest signature before parsing
artifact metadata, then verifies artifact sizes and digests after download. It
refuses non-macOS-arm64 platforms, root execution, non-HTTPS URLs, malformed or
unexpected filenames, a missing exact-version stock Muse prerequisite, and an
unsafe curl credential file. The staged gateway runs its self-test; the staged
launcher must successfully report the expected stock Muse version. Installation
stages files on the destination filesystem, installs the gateway first and the
user-facing launcher last, and retains one previous copy of each for recovery.

Release generation and verification reject a manifest that attempts to include
the stock Muse binary. The release manifest also signs the repository license
and required third-party notice payloads.

### Operational requirements

- Generate and use the signing key only in protected release infrastructure.
- Deliver the public key independently from the release feed; a key fetched
  from a compromised feed is not a trust anchor.
- Publish immutable, access-controlled HTTPS URLs and retain build provenance.
- Inspect the generated bundle before upload and confirm it contains no stock
  Muse binary, signing key, provider credential, or private feed credential.

## Data flow and retention

Prompts, repository excerpts, images, and tool results sent to the selected
OpenAI endpoint are subject to the terms and retention policy of the chosen
account and authentication mode. The gateway requests `store: false`; that flag
does not override provider security, abuse-monitoring, or legal-retention
policies. Local Muse sessions retain their ordinary content.

The isolated Muse profile has telemetry disabled, and this project does not
currently implement a second prompt transcript or an observability pipeline. If
either is introduced, it must be opt-in and exclude prompt bodies, authorization
headers, signed URL query strings, and credentials by default.

The authenticated model catalog crosses the private readiness file only long
enough for the launcher to validate it and atomically seed the isolated Muse
cache. Both files reject symlinks and unsafe ownership or permissions. Missing
release dates and output limits are stored as `null`; the compatibility layer
does not invent provider metadata.

## Verification status

The repository currently automates:

- unit and async fixture coverage for argument routing, auth-home isolation,
  catalog normalization, HTTP behavior, retry policy, SSE validation, bearer
  checks, and readiness-file constraints;
- release manifest generation, signature verification, tamper rejection,
  installer staging, and previous-version retention; and
- formatting and lint checks in CI.

The following remain planned release gates:

- deterministic stock Muse versus wrapped Muse transcript and filesystem
  comparisons;
- approval, sandbox, resume, export, and cancellation regression scenarios; and
- live ChatGPT/API-key authentication, model discovery, streaming, usage, error,
  search, and browser-route checks.

## Residual risks

- A future Muse release can change undocumented endpoint behavior.
- Different models can make materially different tool choices even with a
  compatible transport.
- The stock Muse binary and its updater remain third-party trusted code.
- The ChatGPT backend is private and evolving. The vendored Codex crates are
  open-source, but their Rust library APIs are not a stable integration contract;
  upstream changes require a repin and full retest.
- A malicious local process running as the same user can inspect memory or
  interfere with loopback traffic.
- Proxy or custom-CA configuration can observe provider traffic.
- The absence of completed black-box and live release gates leaves integration
  properties unverified.
- Compromised release infrastructure can sign a malicious wrapper.

Pin supported versions, preserve black-box fixtures, rotate signing and access
credentials, and fail closed when an invariant cannot be established.

## Reporting a vulnerability

Use GitHub's [private vulnerability reporting][report-vulnerability] for
security reports. The permanent repository is expected to enable that route; if
it is not yet available, contact the repository owner privately rather than
opening a public issue.

Do not include live credentials, private-feed URLs, prompts, or proprietary Muse
artifacts. Include the affected `muse-codex` version, stock Muse version,
platform, impact, and a redacted reproduction.

[report-vulnerability]: https://github.com/Srimi1/muse-codex/security/advisories/new
