# Security model

## Security contract

The provider swap must not weaken Muse's existing approval, sandbox, workspace,
or session behavior. It also introduces a stricter provider boundary:

- OpenAI credentials are visible only to `muse-codex` authentication code and
  the local gateway.
- The stock Muse child receives an ephemeral loopback token, never an OpenAI
  token.
- Prompts and model responses do not go to Meta model, authentication, or model
  catalog endpoints during a wrapped run.
- Authentication failures never silently change provider or billing mode.
- A retry cannot duplicate a tool call or another externally visible action.
- Release artifacts are accepted only after signature, platform, size, and
  digest verification.

These are release gates, not optional hardening recommendations.

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
             out-of-band public key
```

The local operating-system account is trusted to read its own processes and
files. Root, debuggers, injected dynamic libraries, and a fully compromised user
account are outside this threat model.

## Protected assets

- ChatGPT/Codex authorization tokens and refresh material
- OpenAI API keys
- private-feed credentials and signed release URLs
- prompts, repository content, tool outputs, images, and session history
- the integrity of tool call IDs and arguments
- Muse approval decisions and sandbox configuration
- the release signing private key

## Credential handling

### OpenAI credentials

- Persist credentials through the upstream Codex keyring implementation only;
  do not write a plaintext `auth.json`.
- Use a stable canonical Muse Codex auth home, defaulting to the platform data
  directory under `muse-codex/codex-home`. `MUSE_CODEX_HOME` may override it.
- Namespace the keyring entry by that canonical path so it does not collide with
  stock Codex state in `~/.codex`.
- Remove `CODEX_ACCESS_TOKEN`, `CODEX_API_KEY`, `CODEX_HOME`,
  `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`,
  `OPENAI_ORGANIZATION`, and `OPENAI_PROJECT` from inherited child
  environments. An invocation-only `OPENAI_API_KEY`, when present, is consumed
  once by the gateway over stdin and then cleared from launcher memory.
- Accept API keys through bounded stdin, never a command-line value.
- Do not put credentials in child argv, URLs, diagnostics, traces, session
  exports, panic reports, or structured errors.
- Redact authorization headers and token-shaped values before logging.
- Keep subscription and API-key modes tagged; never infer one from the presence
  of the other.
- Reject a custom base URL for ChatGPT subscription auth. It is valid only for
  explicitly selected API-key auth.
- Delete only the namespaced `muse-codex` keyring entry on logout.

Browser login exposes an authorization URL and normally opens a localhost
callback listener. Device authorization exposes a verification URL and user
code. Treat both as sensitive while the flow is pending; do not include them in
persistent logs.

### Muse child credential

Each invocation receives a cryptographically random bearer token scoped to its
loopback gateway. The token is short-lived, is not persisted, and is removed
when the child exits. The gateway rejects a missing or mismatched token before
reading a request body.

### Private-feed credentials

The installer requires exact manifest and signature URLs plus an out-of-band
public-key file. Feed authentication belongs in a mode-`0600` curl config file
referenced by `MUSE_CODEX_RELEASE_CURL_CONFIG`; do not place a bearer token in a
URL or shell history. The installer supplies URLs to curl over standard input so
they are not exposed in curl's process arguments.

The release signing private key must live only in protected release
infrastructure. It is never accepted by or copied into the installer.

## Network controls

- Bind the gateway to `127.0.0.1` on an ephemeral port. Do not bind wildcard,
  LAN, Unix-socket, or externally reachable listeners by default.
- Authenticate every Muse-to-gateway request.
- Send upstream requests only over HTTPS with certificate verification enabled.
- Do not follow a redirect that downgrades HTTPS or changes to an unapproved
  provider host.
- Remove `META_API_KEY` and untrusted Muse provider/base-URL variables from the
  child environment.
- Treat proxy variables as explicit configuration because a proxy can observe
  prompts and credentials.
- Apply request-size, header-size, event-size, and idle-time limits.
- Strip hop-by-hop headers before returning upstream Responses to Muse.

Provider-backed web or browser routes must use an explicitly implemented and
tested OpenAI-compatible path. They may not fall back to Meta simply to preserve
the appearance of feature parity.

## Tool-call integrity

Tool execution remains in stock Muse. The gateway must preserve tool name,
arguments, call ID, response ID, and event ordering exactly across translations.
It must not repair malformed JSON heuristically.

Retries are permitted only before any response item has become observable to
Muse. After a tool call or output item is observable, a disconnect is ambiguous
and must terminate the attempt as incomplete. Retrying at that point could cause
duplicate file changes or shell commands.

Cancellation must close the upstream stream and suppress later events. It does
not assert that an upstream service forgot already received prompt data.

## Stock Muse safety boundary

The wrapper does not reimplement or bypass Muse approval and sandbox controls.
Security regression tests compare stock and wrapped behavior for command
approval, writes outside the workspace, network access, resume, and export.

Muse hooks and MCP servers retain their native behavior. In particular, they can
run outside portions of Muse's command sandbox and may access the network. A
trusted provider does not make an untrusted hook, MCP server, skill, plugin, or
repository safe. Keep workspace trust enabled and review these extensions.

## Release and installer security

The private release manifest is signed with an Ed25519 SSHSIG key through the
platform `ssh-keygen` implementation and contains SHA-256 artifact digests plus:

- schema and product identifiers;
- release and minimum-tested-Muse versions;
- the `macos_arm64` launcher and gateway filenames and immutable HTTPS URLs;
- both exact byte lengths and SHA-256 digests; and
- a publication timestamp.

`scripts/install.sh` verifies the detached manifest signature before parsing
artifact metadata, then verifies both sizes and digests before installation. It
refuses non-macOS-arm64 platforms, root execution, non-HTTPS URLs, malformed
filenames, and a missing stock Muse prerequisite. Both downloaded executables
must pass a staged self-test before installation. Installation stages both
binaries on the destination filesystem, installs the gateway first and launcher
last, and retains one previous copy of each for recovery.

The signature protects metadata integrity; HTTPS and feed authorization protect
confidentiality and availability. A public key downloaded from the same
potentially compromised feed is not a trust anchor, which is why the installer
requires a separately delivered key file.

The release bundle must never contain the stock Muse binary. Operators should
inspect the generated directory before upload and retain build provenance for
both project executables.

## Data flow and retention

Prompts, repository excerpts, images, and tool results sent to the chosen OpenAI
model are subject to the terms and retention policy of the selected OpenAI
account and authentication mode. Local Muse sessions retain their ordinary
content. `muse-codex` should avoid creating a second prompt transcript.

Private feeds and observability systems must not log authorization headers or
signed URL query strings. Structured telemetry should be opt-in and must exclude
prompt bodies and credentials by default.

## Residual risks

- A future Muse release can change undocumented endpoint behavior.
- Different models can make materially different tool choices even with a
  compatible transport.
- The stock Muse binary and its updater remain third-party trusted code.
- The ChatGPT backend and pinned Codex Rust crates are private, unstable
  interfaces; upstream changes require a repin and full retest.
- A malicious local process running as the same user can inspect memory or
  interfere with loopback traffic.
- Subscription authorization behavior can change upstream.
- Compromised release infrastructure can sign a malicious wrapper.

Pin supported versions, preserve black-box fixtures, rotate signing and access
credentials, and fail closed when an invariant cannot be established.

## Reporting a vulnerability

Do not include live credentials, private-feed URLs, prompts, or proprietary Muse
artifacts in a public report. Contact the repository owner through a private
security channel and include the affected `muse-codex` version, stock Muse
version, platform, and a redacted reproduction.
