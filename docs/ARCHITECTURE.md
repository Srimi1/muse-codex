# Architecture

## Objective

`muse-codex` changes the model provider while retaining the stock Muse Code
harness. It is an external compatibility layer, not a Muse fork. This boundary
is necessary because Meta publishes the Muse Session Protocol SDK and schema,
but not the Rust host that implements the terminal agent.

The supported Muse baseline is `1.0.3-R2198.1`. The Codex client and wire
compatibility version are pinned to `rust-v0.153.4`; dependency upgrades and
Muse upgrades are separate compatibility events.

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
- remove the wrapper-owned, launch-scoped `--fast` selector before stock Muse
  parses its arguments;
- bind the gateway to an ephemeral `127.0.0.1` port;
- mint a per-run bearer token for the Muse-to-gateway hop;
- seed Muse's isolated normalized model cache from the authenticated catalog;
- inject the gateway base URL and internal provider protocol selector;
- remove provider-routing and credential environment variables that the Muse
  child must not inherit; and
- forward signals and exit status.

Non-provider argument values are preserved, including literal arguments after
`--`. The launcher places `exec` and `resume` before their startup options,
because the pinned Muse parser otherwise fails to dispatch `exec`. `serve`
receives its endpoint through isolated settings rather than unsupported flags.
For MSP, a schema-aware JSONL relay maps only provider fields and retains
request IDs. It rejects explicit unsupported providers and duplicate IDs without
rewriting prompts, tool payloads, or extension data. The stable schema is
unchanged. `serve --no-session-log` fails before startup because the pinned
host cannot deliver turn events without durable session storage.

User-supplied provider or base-URL flags are rejected or replaced deterministically; they
cannot be permitted to bypass the gateway.

Model arguments pass through unchanged. When no model was specified, stock Muse
selects the default marked in the authenticated catalog cache. Resume and model
switch behavior therefore remain under the stock harness rather than being
overridden by the launcher.

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

- `GET /muse-code/models`: return `304 Not Modified` after the launcher has
  installed the authenticated catalog in Muse's isolated normalized cache; and
- `POST /responses`: forward a Muse Responses request through the pinned Codex
  client and validate the upstream event stream before returning it to Muse.

The gateway does not maintain a local model allowlist. Before signaling
readiness it fetches the authenticated catalog, sorts on upstream priority,
marks the first picker-visible result as the default, and includes the result in
its private readiness document. The launcher atomically converts that document
to Muse 1.0.3's normalized cache format. This cache path is necessary because
the pinned Muse raw-catalog decoder cannot retain multiple rows whose optional
release date or output limit is unknown. Unknown values remain JSON `null`;
they are never replaced with fabricated limits or dates. Authenticated
reasoning-effort choices are retained in upstream order; Muse's cache has no
provider-default effort field, so no default effort is invented.

Fast mode is a provider setting owned by the launcher and gateway. When
`--fast` is selected, it applies to every Responses turn for that TUI, `exec`,
`resume`, or `serve` process; stock Muse 1.0.3 has no service-tier field or
`/fast` command. For ChatGPT, the transport accepts Fast only when the selected
catalog model advertises the `priority` tier or the pinned legacy `fast`
capability, emits the canonical `service_tier: "priority"` field, and supplies
the trusted routing hint. A custom API-key endpoint receives that tier without
first-party catalog gating.
Standard launches remove any inbound tier and omit the field. Fast remains
orthogonal to Ultra or any other reasoning effort. The launcher reports it as
requested rather than guaranteed because upstream can downgrade the effective
service tier; priority processing also increases usage or cost.

The launcher opens Muse 1.0.3's built-in Ultra feature gate only for the
isolated stock-Muse runtime. Muse therefore owns Ultra's proactive workflow and
subagent delegation behavior. Consistent with the pinned Codex client, Ultra
uses the model catalog's multi-agent wire effort (`xhigh` for GPT-6 Astra)
instead of forwarding a literal `ultra` API value. This preserves the
provider-only boundary: orchestration stays in Muse and transport stays in the
gateway.

The catalog parser honors each model's minimum client version, picker
visibility, input modalities, reasoning-effort list, Responses Lite flag, and
tool mode. It understands the current entries for GPT-6 Astra and GPT-5.6 Sol,
Terra, and Luna, but exposes them only when the authenticated endpoint returns
them. A model requiring a newer wire version, an unknown tool mode, non-text
input only, or no recognized effort is hidden rather than guessed into
compatibility.

Catalog discovery has a 90-second startup deadline, while the launcher allows
100 seconds for the complete readiness handshake. This keeps slow credential
refresh and network startup bounded without racing the catalog request.

For Responses requests, the gateway enforces streaming and sets `store` to
`false`. When the selected catalog entry requests Responses Lite, the adapter
adds the internal Lite capability header, moves tool definitions into a stable
`additional_tools` developer item, moves base instructions into a developer
message, requests all-turn reasoning context, and disables parallel tool calls
as required by that wire contract. Direct Muse function/custom tools are
grouped into the Lite `functions` namespace. Muse still supplies complete
history and remains the sole owner of the agent and tool-execution loop.

Valid, known non-metadata SSE frames are forwarded without rewriting their
response IDs, item IDs, call IDs, tool names, JSON arguments, or ordering. The
validator supports fragmented text and function-argument events, recognizes
`response.cancelled` as terminal, and drops `codex.response.metadata` alongside
other known transport metadata. Malformed, oversized, idle, interrupted, or
unknown event streams become a terminal `response.failed` event instead of
being reinterpreted as text or exposed as a retryable bare EOF.

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
| `auth set --provider codex --api-key-stdin` | Bounded, non-empty API key from stdin; strip only trailing CR/LF |
| `logout` | Remove only the namespaced Muse Codex keyring entry |
| `auth status` | Report the isolated saved credential mode without starting Muse |
| `exec --api-key-stdin` | Use a bounded invocation-only key through the private pipe |

| `auth set --provider zai --api-key-stdin` | Store a Z.ai key in its own keyring record |

Credentials use the upstream Codex keyring store, never a plaintext
`auth.json`. The default isolated Codex auth home is the platform data directory
under `muse-codex/codex-home`, overridable with `MUSE_CODEX_HOME`. Its canonical
path determines a stable, namespaced keyring account so it does not collide with
`~/.codex`.

Z.ai credentials live in a separate record: service `Muse Codex Z.ai`, account
`zai|<first 16 hex of sha256 of the canonical isolated home>`. Both the service
and the account prefix differ from the upstream Codex record (`Codex Auth` and
`cli|<digest>`), so neither a service nor a digest collision can let one
provider's credential satisfy the other. Deriving the account from the same home
keeps separate `MUSE_CODEX_HOME` profiles isolated from each other as well.

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
receives only the random loopback credential. The Z.ai provider is always
API-key authenticated, so its base URL override is unconditional but still
HTTPS-only with no credentials, query, or fragment.

## Provider backends

`codex-transport` owns both upstreams behind a `Backend` enum that the gateway
holds in place of a bare `Transport`. It is an enum rather than a trait object
so adding a route or a provider forces an explicit decision for every
combination; a defaulted trait method is exactly the shape that produces a
silent fallback between providers. Exactly one provider is selected per gateway
process, named on the `serve` command line, and echoed in the private readiness
file so the launcher can refuse a gateway that served a different upstream than
it asked for.

The Z.ai backend lives in `codex-transport::zai`. Z.ai speaks OpenAI
chat-completions rather than the Responses API, so that module translates a Muse
Responses request into a Z.ai chat request and synthesizes a Responses event
stream back. The synthesized stream is then piped through the same SSE validator
every upstream stream passes, so a synthesizer bug degrades to a terminal
`response.failed` rather than corrupt output reaching Muse.

Z.ai publishes no model-listing endpoint, so its catalog is pinned in the source
rather than fetched. At startup the backend calls Z.ai's plan-usage endpoint,
which confirms the credential and an active plan without spending a coding
prompt; the usage figures it returns are never parsed, logged, or written to the
readiness file.

`codex-transport` is now the shared home for both providers as well as the
vocabulary types (`Error`, `RawBody`, `RawResponse`, `ModelInfo`) the gateway
depends on. If a third provider is added, extract those types into a neutral
crate rather than growing this one further.

## Command routing

| Invocation | Owner | Result |
| --- | --- | --- |
| `muse-codex login` | launcher/auth | OpenAI browser authorization |
| `muse-codex login --device-auth` | launcher/auth | OpenAI device authorization |
| `muse-codex auth set --provider codex --api-key-stdin` | launcher/auth | Store a bounded API key from stdin |
| `muse-codex logout` | launcher/auth | Remove only `muse-codex` credentials |
| auth help or invalid auth forms | launcher | Show Codex auth help or a usage error without touching Meta auth |
| local commands and help/version | stock Muse parser | Run offline in the isolated profile; provider help labels describe Codex |
| `muse-codex [--fast] [Muse arguments]` | launcher then stock Muse | Start gateway and pass through; optionally request Fast for the whole process |
| `muse-codex --provider zai [Muse arguments]` | launcher then stock Muse | Start the Z.ai-backed gateway and pass through |
| provider other than `codex` or `zai` | launcher | Reject; `meta` is internal only |
| two different `--provider` values | launcher | Reject as ambiguous rather than taking the last one |
| `--fast` with `--provider zai` | launcher, then gateway | Reject; GLM has no service tier |
| base-URL override | launcher/gateway | API-key-only under `codex`, unconditional under `zai`; Muse still receives loopback |

## Turn lifecycle

1. The launcher validates the stock Muse binary and selected authentication
   mode before starting a session.
2. It starts the loopback listener; the gateway fetches the authenticated model
   catalog before reporting readiness.
3. The launcher validates that catalog, atomically seeds Muse's isolated model
   cache, and launches Muse with the loopback endpoint and ephemeral bearer
   token.
4. Muse retains the seeded catalog after the gateway answers its conditional
   catalog request with `304`, then sends a Responses request with the selected
   model.
5. The gateway validates that a subscription model and reasoning effort are
   present in that authenticated catalog and, when requested, that the model
   advertises the priority tier. It applies Fast and the catalog-selected
   standard Responses or Responses Lite mapping, then streams typed events
   back with backpressure.
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

- Final upstream Responses HTTP failures become a sanitized terminal SSE
  failure. The pinned Muse host retries bare HTTP errors, even HTTP 400;
  translating the final failure prevents a second retry loop in the harness.
  Rate-limit and request headers pass through the response-header allowlist.
  Search and browser failures use bounded gateway errors.
- Isolated Muse settings use `provider_retry.max_retries = 0` so the stock
  host cannot add a second provider retry loop to the transport's budget.
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
MSP translation, and gateway invariants, plus shell tests for release generation,
verification, and installation. The repository records the supported Muse
schema/help baseline. `tests/scripts/cli-msp-parity-test.py` additionally compares
the exact stock and wrapped executables against a deterministic endpoint:
command routing, text, tool/result loops, terminal failures, request counts, and
MSP turn events. It requires a separately installed Muse binary and runs outside
public CI. Selected live subscription flows are also tested locally, not in CI.

A supported release requires broader qualification across three lanes:

1. stock Muse against a deterministic scripted endpoint;
2. wrapped Muse against the same endpoint through the gateway; and
3. wrapped Muse against the real OpenAI service.

The first two lanes must expand to compare normalized MSP transcripts, session exports,
hook logs, exit codes, and filesystem effects. Timestamps, random IDs, and
provider model names may be normalized; safety decisions and tool-call identity
must not be. The full live gate validates real authentication, streaming,
cancellation, model discovery, usage, and error behavior.

Feature parity means the provider-independent harness behavior is unchanged. It
does not mean two different models produce identical prose or tool choices.

## Configuration ownership

| State | Owner |
| --- | --- |
| Muse settings, sessions, approvals, rules, and extensions | stock Muse |
| OpenAI credentials and selected auth mode | `muse-codex` keyring namespace |
| Ephemeral listener address and bearer token | launcher process |
| Model selection | stock Muse and user-supplied arguments, using authenticated upstream defaults |
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
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a` (`rust-v0.153.4`), but their library
APIs are not a stable compatibility contract for this project. A repin requires
the complete auth, transport, error, cancellation, and compatibility suite;
semver compatibility must not be assumed.

## Release boundary

The release unit contains the `muse-codex` launcher and `muse-codex-gateway`
macOS arm64 executables, a signed manifest and signature, the project license,
the third-party notices, and the vendored Codex notice. The manifest records
sizes and digests for both executables and every legal payload. The stock Muse
binary is deliberately absent. `scripts/install.sh` checks for Muse as a
prerequisite and installs only the two verified project binaries.
