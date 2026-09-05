# CLI and protocol compatibility

The supported host is exactly Muse Code `1.0.3-R2198.1`. Muse Codex adapts
provider concerns at the process, HTTP, and MSP boundaries. The stock binary
continues to execute tools, enforce approvals, and own sessions.

## Command dispatch

| Interface | Invocation | Output |
| --- | --- | --- |
| Terminal UI | `muse-codex` | Stock Muse TUI using the authenticated catalog |
| Headless turn | `muse-codex exec --json "prompt"` | Stock session-event JSONL |
| MSP host | `muse-codex serve` | JSON-RPC frames over stdin/stdout |
| Stable schema | `muse-codex schema generate-json-schema --out DIR` | Unmodified stock MSP schema bundle |
| Credentials | `muse-codex auth status` | Isolated OpenAI authentication status |

The pinned parser requires `exec` before startup options. `serve` accepts no
root provider flags, so it reads the isolated endpoint settings. The launcher
handles this distinction; it must never start the terminal UI for a protocol
command. Help, schema export, and other local commands do not start a gateway
or require provider credentials.

`serve --no-session-log` is rejected before startup. The pinned host accepts
this flag but fails to deliver MSP turn events in memory-only mode. Use normal
`serve`; its durable sessions remain in the isolated Muse Codex profile.
Headless `exec --no-session-log` is still supported.

## MSP provider fields

MSP clients select `codex` in `session/start.params.providerId` or
`session/setModel.params.model.providerId`. The launcher translates those
fields to the internal provider selector understood by Muse. Other explicit
providers are rejected with a correlated protocol error.

The response adapter translates the corresponding provider fields in session
results, catalog results, model-change notifications, and effective-model
snapshots. It tracks request IDs so parallel requests retain their original
correlation. It does not search or replace provider names inside prompt text,
tool arguments, tool output, or extension data.

The stable schema defines provider identifiers as strings, so these label
changes do not require changing its bytes or fingerprint. Session event files
and `exec --json` remain the stock harness's records and can contain internal
provider labels.

## Responses requests and streams

Muse sends complete history. The transport normalizes message content into
the pinned Responses item types, preserves call IDs and tool results, and
requests `store: false` with streaming enabled. Provider-side conversation
handles are rejected because they would introduce a second history source.
API-key requests retain supported public API parameters; subscription requests
omit parameters unsupported by the pinned ChatGPT backend.

The wrapper-owned `--fast` option applies to every request for the launched
TUI, `exec`, `resume`, or `serve` process. Stock Muse 1.0.3 has no service-tier
field or `/fast` command, so this setting is intentionally absent from MSP and
session records. For ChatGPT, the selected catalog model must advertise the
priority tier or the pinned legacy `fast` capability; the gateway sends
`service_tier: "priority"` plus the trusted routing hint. Explicit custom
API-key endpoints receive the priority request without catalog gating. A
standard launch removes any inbound
`service_tier` instead of trusting it. Fast is independent of reasoning effort
and may be combined with Ultra. It increases usage or cost, and the startup
banner says it was requested because upstream may report a downgraded effective
tier.

The authenticated subscription catalog determines both model visibility and
the request dialect. Catalog entries marked `use_responses_lite` receive the
Responses Lite header and wire shape used by current Codex clients: tools and
base instructions become stable developer input items, direct
function/custom tools are grouped under the `functions` namespace,
`reasoning.context` is `all_turns`, and parallel tool calls are disabled.
This is a transport mapping only; Muse still assembles history, executes tools,
and decides when the next model turn starts.

The `rust-v0.153.4` wire parser understands authenticated catalog entries for
`gpt-6-astra`, `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`. It does not
hard-code those entries into a user's picker. Subscription requests are
accepted only for a picker-visible model returned by that account's catalog,
with an effort advertised for that model. Rows requiring a newer client wire
version or an unknown transport capability are hidden instead of being
presented as usable.

Stock Muse records namespaced calls as dotted names such as `muse.read_file`.
The adapter restores the separate namespace/name fields only for exact matches
in the advertised tool catalog. It preserves call IDs, arguments, results, and
ordering, and rejects ambiguous aliases instead of guessing.

The gateway validates fragmented SSE before exposing it to Muse. It recognizes
`response.cancelled` as a terminal event and drops
`codex.response.metadata` as known transport metadata. Malformed, unknown
non-metadata, interrupted, or idle streams end with a terminal provider
failure. A final upstream HTTP failure is also translated into a terminal
failure: stock Muse otherwise retries even rejected HTTP 400 requests. The
gateway never replays an observable stream or executes a tool itself.
The isolated host settings disable Muse's additional provider retry loop;
the gateway owns the bounded pre-stream retry budget.

Startup errors use a private, bounded error-code document. Only fixed
diagnostics reach the launcher; credentials and upstream error bodies are not
copied into protocol output.

## Local verification (2026-09-05)

- The first-party Rust suites and all-target Clippy with warnings denied pass.
- The exact Muse help/version baseline and stable MSP schema hashes match.
- Deterministic stock/wrapped tests pass for headless text, tools, terminal
  failures without replay, MSP turns, process-restart resume, and fork history.
- Current-catalog fixtures cover GPT-6 Astra and GPT-5.6 Sol, Terra, and Luna,
  including minimum-client, visibility, effort, modality, tool-mode, and
  Responses Lite mapping rules.
- Live ChatGPT subscription `exec --json` completes a real file-read tool loop.
- Live MSP completes one file-read tool loop, restarts the host, resumes the
  saved session, and recalls the result without another tool call.

The live test is opt-in, refuses API-key credentials, preserves the default
sandbox, disables shell and writes, and prints no protocol frames or account
details. It creates one dedicated durable test session in the selected profile:

```sh
python3 tests/scripts/live-msp-subscription-smoke.py --run \
  --wrapper target/debug/muse-codex \
  --gateway target/debug/muse-codex-gateway
```

Successful smoke tests are evidence for these paths only, not blanket feature
or security certification. API-key billing is not exercised by this live test.

## Qualification limits

The original plan required a modifiable Muse host source tree. That source is
not in this repository, so this project is a compatibility gateway rather
than a source fork. Unit tests, fixture tests, and successful individual live
turns do not establish parity for every Muse feature.

Full approval/sandbox/hook/plugin/worktree regressions, every auxiliary
provider route, and cross-account concurrent catalog behavior remain release
qualification work. Use separate `MUSE_CODEX_HOME` profiles for concurrent
invocations with different accounts or upstream endpoints. Private release
publication also requires a release-feed URL and signing material.

The Codex repin changed the dependency graph. Known version-flagged dependencies
and their current helper reachability are recorded in the
[pinned dependency review](DEPENDENCY_REVIEW.md), but that document is not a
substitute for a fresh advisory scan before release.
