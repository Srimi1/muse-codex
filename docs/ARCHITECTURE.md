# Architecture

## Objective

`muse-codex` changes the model provider while retaining the stock Muse Code
harness. It is an external compatibility layer, not a Muse fork. This boundary
is necessary because Meta publishes the Muse Session Protocol SDK and schema,
but not the Rust host that implements the terminal agent.

The supported Muse baseline is `1.0.3-R2198.1`. The Codex client is pinned by the
workspace dependency graph; dependency upgrades and Muse upgrades are separate
compatibility events.

This document describes current behavior in the present tense. Statements
labelled as release requirements or planned gates describe work that must be
verified before a supported release; they are not claims about the current test
suite.

## Component model

```text
+------------------+       +----------------------+       +------------------+
| muse-codex       |       | stock Muse Code      |       | OpenAI           |
|                  |       |                      |       |                  |
| CLI routing      | spawn | TUI and exec mode    | HTTP  | Codex/GPT models |
| auth controller  +------>+ sessions/event log   +--+    | Responses API    |
| gateway lifecycle|       | tools and approvals  |  |    +---------^--------+
+---------+--------+       | sandbox/extensions   |  |              |
          |                +----------------------+  |              |
          |                                          v              |
          |                               +---------------------+    |
          +------------------------------>+ loopback gateway    +----+
                                          | catalog + responses|
                                          | stream translation |
                                          +---------------------+
```

### Launcher

The launcher owns only process and provider concerns:

- intercept the `auth` command family;
- locate and validate a separately installed stock `muse` executable;
- load one explicit OpenAI authentication mode;
- bind the gateway to an ephemeral `127.0.0.1` port;
- mint a per-run bearer token for the Muse-to-gateway hop;
- inject the gateway base URL and the internal provider protocol selector;
- remove provider-routing and credential environment variables that the Muse
  child must not inherit; and
- forward signals and exit status.

All non-provider CLI arguments pass through byte-for-byte. User-supplied
provider or base-URL flags must be rejected or replaced deterministically; they
cannot be permitted to bypass the gateway.

The launcher does not select, inject, or rewrite a model. An explicit Muse model
argument passes through unchanged; otherwise stock Muse selects from the model
catalog returned by the gateway.

### Stock Muse child

The stock executable remains responsible for:

- interactive and headless interfaces;
- prompt and project-context assembly;
- session storage, export, replay, resume, fork, and compaction;
- tool schemas, tool execution, approvals, and OS sandboxing;
- `AGENTS.md`/`CLAUDE.md` precedence and workspace trust;
- skills, hooks, MCP servers, plugins, goals, workflows, subagents, and
  worktrees; and
- model-independent usage presentation and cancellation controls.

The wrapper neither copies nor links Muse code. The user installs and licenses
Muse independently.

### Compatibility gateway

Muse connects to the gateway using its existing endpoint transport. At minimum,
the gateway exposes:

- `GET /muse-code/models`: fetch the authenticated upstream model catalog and
  normalize it into Muse's catalog shape; and
- `POST /responses`: forward a Muse Responses request through the pinned Codex
  client and validate the upstream event stream before returning it to Muse.

The gateway does not maintain a local model allowlist. Model availability and
metadata originate upstream; the normalizer sorts on upstream priority, marks
the first result as the Muse default, and omits optional limits that upstream
does not provide instead of inventing them.

For Responses requests, the gateway enforces streaming and sets `store` to
`false`. Valid, known non-metadata SSE frames are forwarded without rewriting
their response IDs, item IDs, call IDs, tool names, JSON arguments, or ordering.
The validator supports fragmented text and function-argument events. It drops
known transport metadata events and converts malformed, oversized, idle,
interrupted, or unknown event streams into a terminal `response.failed` event
instead of reinterpreting them as text or exposing a retryable bare EOF.

The gateway does not execute model-requested tools. Muse receives the function
call, performs its normal approval and sandbox flow, executes the tool, and
sends the result on the next model request.

### Authentication controller

Subscription login and API-key authentication are separate credential types and
billing paths. The controller supports:

| Command | Behavior |
| --- | --- |
| `login` | Browser authorization with local callback |
| `login --device-auth` | Verification URL and user-code flow |
| `auth set --provider codex --api-key-stdin` | Bounded, trimmed, non-empty API key from stdin |
| `logout` | Remove only the namespaced Muse Codex keyring entry |

Credentials use the upstream Codex keyring store, never a plaintext
`auth.json`. The default isolated Codex auth home is the platform data directory
under `muse-codex/codex-home`, overridable with `MUSE_CODEX_HOME`. Its canonical
path determines a stable, namespaced keyring account so it does not collide with
`~/.codex`.

Before constructing auth or transport, the process removes `CODEX_ACCESS_TOKEN`,
`CODEX_API_KEY`, `CODEX_HOME`, `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`,
`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`, and `OPENAI_PROJECT`
from inherited child environments. An invocation-only API key is delivered to
the gateway over stdin instead. This is required because upstream Codex auth
gives some environment credentials precedence even when keyring mode was
requested.

A custom OpenAI base URL is valid only for API-key authentication. Subscription
authorization is tied to the approved ChatGPT backend and rejects a custom base
URL. The OpenAI credential is consumed by the gateway only; the Muse child
receives only the random loopback credential.

## Command routing

| Invocation | Owner | Result |
| --- | --- | --- |
| `muse-codex login` | launcher/auth | OpenAI browser authorization |
| `muse-codex login --device-auth` | launcher/auth | OpenAI device authorization |
| `muse-codex auth set --provider codex --api-key-stdin` | launcher/auth | Store a bounded API key from stdin |
| `muse-codex logout` | launcher/auth | Remove only `muse-codex` credentials |
| other auth/help forms | stock Muse parser | Preserve stock validation and help in the isolated profile |
| `muse-codex [Muse arguments]` | launcher then stock Muse | Start gateway and pass through |
| provider other than `codex` | launcher | Reject; `meta` is internal only |
| base-URL override | launcher/gateway | Treat as API-key-only upstream; Muse still receives loopback |

## Turn lifecycle

1. The launcher validates the stock Muse binary and selected authentication
   mode before starting a session.
2. It starts the loopback listener and waits until it is ready.
3. It launches Muse with the loopback endpoint and ephemeral bearer token.
4. Muse obtains the upstream-derived catalog and sends a Responses request with
   its selected model.
5. The gateway authenticates upstream, maps the request, and streams typed
   events back with backpressure.
6. Muse renders output or performs its ordinary tool approval/execution loop.
7. When Muse drops a response stream, the associated upstream stream is
   dropped. End-to-end cancellation behavior remains a release gate; the
   gateway does not retry a stream after exposing its response body.
8. On child exit, the launcher stops the gateway, releases credentials from
   memory, and returns Muse's exit status.

Raw upstream Responses retain their status, headers, and body bytes until the
gateway maps them. Hop-by-hop response headers are removed before forwarding to
Muse.

## Retry and failure rules

- Non-success upstream Responses statuses and bodies are forwarded through the
  gateway's response-header allowlist. Failures raised inside catalog, search,
  browser, or pre-stream transport handling return bounded gateway errors.
- The current transport makes at most two pre-stream attempts for retryable send
  failures or upstream 5xx responses and can perform upstream Codex 401
  credential recovery. It does not retry after the response body has been
  returned to Muse.
- Once a stream is observable, an ambiguous disconnect becomes a terminal
  `response.failed` event; the gateway does not retry and risk duplicate local
  tool execution.
- Malformed or unknown provider events fail the turn with a bounded diagnostic.
- The launcher never falls back to a direct Meta endpoint or a different OpenAI
  billing mode.
- A supported release must verify that provider failure leaves Muse's
  append-only session recoverable.

## Compatibility strategy

Current automated coverage consists of Rust unit and asynchronous fixture tests
for routing, authentication, transport, catalog normalization, SSE validation,
and gateway invariants, plus shell tests for release generation, verification,
and installation. The repository also records the supported Muse schema/help
baseline. This coverage does not yet constitute an end-to-end comparison with
stock Muse or a live OpenAI run.

A supported release requires three additional compatibility gates:

1. stock Muse against a deterministic scripted endpoint;
2. wrapped Muse against the same endpoint through the gateway; and
3. wrapped Muse against the real OpenAI service.

The planned first two gates compare normalized MSP transcripts, session exports,
hook logs, exit codes, and filesystem effects. Timestamps, random IDs, and
provider model names may be normalized; safety decisions and tool-call identity
must not be. The planned live gate validates real authentication, streaming,
cancellation, model discovery, usage, and error behavior.

Feature parity means the provider-independent harness behavior is unchanged. It
does not mean two different models produce identical prose or tool choices.

## Configuration ownership

| State | Owner |
| --- | --- |
| Muse settings, sessions, approvals, rules, and extensions | stock Muse |
| OpenAI credentials and selected auth mode | `muse-codex` keyring namespace |
| Ephemeral listener address and bearer token | launcher process |
| Model selection | stock Muse and user-supplied Muse arguments |
| Upstream model discovery and protocol compatibility | gateway |
| Private release URL and curl credentials | release operator/user |

The launcher uses per-process flags plus a persistent, isolated stock-Muse
profile under the muse-codex application-data directory. It seeds that
profile's endpoint transport and disables telemetry, but never reads or writes
`~/.config/muse/settings.json`, `~/.config/muse/auth.json`, or stock Codex
state.

## Upstream stability boundary

The ChatGPT backend is a private, evolving service interface. The Codex Rust
crates used here are open-source and vendored from commit
`9474e5cfc4494b0ba319352aa86ce436c59e65c8`, but their library APIs are not a
stable compatibility contract for this project. A repin requires the complete
auth, transport, error, cancellation, and compatibility suite; semver
compatibility must not be assumed.

## Release boundary

The release unit contains the `muse-codex` launcher and `muse-codex-gateway`
macOS arm64 executables, a signed manifest and signature, the project license,
the third-party notices, and the vendored Codex notice. The manifest records
sizes and digests for both executables and every legal payload. The stock Muse
binary is deliberately absent. `scripts/install.sh` checks for Muse as a
prerequisite and installs only the two verified project binaries.
