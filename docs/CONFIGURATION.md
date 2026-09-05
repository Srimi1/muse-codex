# Configuration

Muse Codex has a deliberately small configuration surface. Defaults favor an
isolated local profile, loopback-only networking, and explicit authentication.

## Authentication modes

| Mode | Command | Storage and billing |
| --- | --- | --- |
| ChatGPT browser | `muse-codex login` | Isolated Codex keyring namespace; ChatGPT account entitlement |
| ChatGPT device | `muse-codex login --device-auth` | Same namespace; device authorization flow |
| OpenAI API key | `muse-codex auth set --provider codex --api-key-stdin` | Keyring; OpenAI API billing |
| Z.ai API key | `muse-codex auth set --provider zai --api-key-stdin` | Separate keyring record; Z.ai GLM Coding Plan |
| Invocation-only API key | Set `OPENAI_API_KEY` (or `ZAI_API_KEY`) for one launch | Sent to the gateway over stdin and removed from the Muse child environment |

Each provider owns a distinct keyring record, so `logout` removes only the
record for the provider named on the command line and never both. Z.ai issues
static keys and has no interactive sign-in, so `muse-codex login --provider zai`
is rejected rather than silently treated as `auth set`.

Use `muse-codex logout` to remove only the Muse Codex credential entry. The
project does not read or delete stock Codex or Muse credentials.

`muse-codex auth status` reports the saved authentication mode without starting
Muse or fetching the model catalog. Authentication help and invalid auth
arguments are handled by Muse Codex itself.

## Runtime environment

| Variable | Purpose | Rules |
| --- | --- | --- |
| `MUSE_CODEX_MUSE_BIN` | Override stock Muse discovery | Absolute or relative path to an executable reporting exactly `1.0.3-R2198.1` |
| `MUSE_CODEX_GATEWAY_BIN` | Override gateway discovery | Must point to an executable gateway binary |
| `MUSE_CODEX_HOME` | Override isolated application state | Absolute path ending in `muse-codex`; parent must exist; cannot be the home directory or live under `~/.codex` |
| `MUSE_CODEX_GATEWAY_READY_TIMEOUT_MS` | Override gateway startup timeout | Positive integer milliseconds; invalid or zero values use the default |
| `OPENAI_API_KEY` | Supply an invocation-only API key | Consumed by the gateway path and removed from the Muse child environment |
| `OPENAI_BASE_URL` | Override the upstream OpenAI-compatible endpoint | Allowed only with explicit API-key authentication and must use HTTPS |
| `ZAI_API_KEY` | Supply an invocation-only Z.ai key | Used only with `--provider zai`; consumed by the gateway and removed from the Muse child |
| `ZAI_BASE_URL` | Override the Z.ai endpoint | Must use HTTPS; use `https://open.bigmodel.cn/api/coding/paas/v4` for the China plan |
| `MUSE_CODEX_ZAI_SKIP_PROBE` | Skip the Z.ai startup plan check | Set to `1`; a bad key then surfaces on the first turn instead of at startup |

The default state directory on macOS is:

```text
~/Library/Application Support/muse-codex
```

It is created with private permissions and contains isolated stock-Muse config
and data directories. It is not stock Muse's normal profile.

Each provider gets its own stock-Muse profile inside that directory
(`stock-config`/`stock-data` for `codex`, `stock-config-zai`/`stock-data-zai`
for `zai`). Muse's normalized catalog cache is keyed only by its internal
provider name, so a shared profile would let one upstream overwrite the other's
catalog and interleave their session histories.

## Command-line routing

- Omitting `--provider` selects `codex`.
- `--provider codex` and `--provider zai` are accepted and removed before Muse
  is started. Naming two different providers in one invocation is rejected
  rather than resolved last-flag-wins.
- Other provider values, including `meta` and `echo`, are rejected at the public
  wrapper boundary.
- `--base-url HTTPS_URL` overrides the upstream endpoint. Under `codex` it is
  API-key-only; under `zai` it is always available because that provider is
  always API-key authenticated. Muse still sees the private loopback URL.
- `--fast` requests OpenAI Fast mode for every model turn in that process. It is
  rejected with `--provider zai`: the GLM Coding Plan has no service tier, and
  accepting a billing-affecting flag that cannot be honored would misreport what
  the user bought.
- Under `--provider zai` the `/muse-code/search` and `/muse-code/browser_open`
  routes answer `501`, because Z.ai offers no equivalent and an empty success
  would let the model reason from a false premise.
- Other arguments pass through to the stock Muse parser.

Use `muse-codex exec --json "prompt"` for headless JSONL output and
`muse-codex serve` for MSP over stdin/stdout. Muse 1.0.3 requires the subcommand
before its startup flags; the launcher places provider flags in the correct
scope. The `serve` command gets its endpoint from isolated settings because
its parser does not accept provider flags.

Fast mode is wrapper-owned and launch-scoped because Muse 1.0.3 has no service
tier in its CLI, session format, or MSP schema and provides no `/fast` toggle.
It therefore applies to the whole TUI, `exec`, `resume`, or `serve` process;
restart with or without `--fast` to change it. For ChatGPT, the selected model
must advertise the `priority` tier or the pinned client's legacy `fast`
capability. The gateway then sends the canonical Responses field
`service_tier: "priority"` and the trusted routing hint. Custom API-key
endpoints receive the priority request without catalog gating and are
responsible for accepting or rejecting it. Standard launches strip and omit any
incoming service tier. The startup message deliberately says Fast was
*requested*: upstream routing can report a downgraded effective tier. Fast
increases usage or cost and is independent of reasoning effort, so
`--fast --reasoning-effort ultra` is valid for a model that supports both.

Use durable sessions for MSP: `serve --no-session-log` is rejected because the
pinned Muse host cannot deliver turn events in that mode. Normal `serve` keeps
sessions in the isolated profile; `exec --no-session-log` remains available.

Local commands (`schema`, `export`, `trace`, `config`, `skills`, `sandbox`,
`session-message`, and `init`) and help/version output work without starting
the gateway. They operate on the Muse Codex profile.

`exec --api-key-stdin` reads an invocation-only key in the launcher and passes
it through the private gateway pipe. It is removed from Muse's arguments and
stdin. An explicit stdin key takes precedence over `OPENAI_API_KEY`; neither
choice changes saved credentials. API-key input is limited to 16 KiB.
Only trailing CR/LF line endings are removed; spaces and other whitespace are
rejected rather than silently changing the credential.

An explicit Muse `--model` argument remains unchanged. Without one, stock Muse
uses the first picker-visible default in the authenticated upstream catalog.
The launcher atomically seeds Muse's isolated normalized catalog cache so
unknown release dates and output limits remain unknown instead of being
fabricated, while authenticated reasoning-effort choices retain upstream order.
The gateway then translates the selected model's Responses stream, selecting
the Responses Lite wire form when the catalog says that model requires it.

Models that advertise Ultra expose it through both `--reasoning-effort ultra`
and the TUI `/effort` picker. Ultra is not an alias for `max`: it activates
Muse's proactive workflow/subagent delegation mode. The model request itself
uses the pinned catalog's multi-agent reasoning effort (`xhigh` for GPT-6
Astra), matching the pinned Codex client rather than inventing an unsupported
literal API effort. Models whose authenticated catalog omits Ultra do not show
the choice.

The current pinned client understands catalog entries for GPT-6 Astra and
GPT-5.6 Sol, Terra, and Luna. This is protocol support, not an entitlement: a
subscription model is usable only when the active account's catalog returns it
as picker-visible. Rows requiring a newer client version or an unknown tool
mode are hidden rather than exposed optimistically.

## Environment isolation

Before authentication or process launch, Muse Codex removes inherited values
that could bypass the selected provider or leak credentials, including:

```text
CODEX_ACCESS_TOKEN
CODEX_API_KEY
CODEX_HOME
CODEX_INTERNAL_ORIGINATOR_OVERRIDE
META_API_KEY
OPENAI_API_KEY
OPENAI_BASE_URL
OPENAI_ORGANIZATION
OPENAI_PROJECT
ZAI_API_KEY
Z_AI_API_KEY
ZHIPUAI_API_KEY
ZAI_BASE_URL
ANTHROPIC_API_KEY
ANTHROPIC_AUTH_TOKEN
ANTHROPIC_BASE_URL
```

The `ANTHROPIC_*` names are on this list because the Z.ai GLM Coding Plan is
commonly wired into other agents through `ANTHROPIC_BASE_URL` and
`ANTHROPIC_AUTH_TOKEN`. A developer's shell therefore often holds a live Z.ai
credential under those names, and it must not reach the tool-executing child.

The launcher later sets a new `META_API_KEY` containing only the random,
short-lived credential for the local Muse-to-gateway hop.

## Private release installer

| Variable | Purpose |
| --- | --- |
| `MUSE_CODEX_RELEASE_MANIFEST_URL` | Exact HTTPS URL of the signed manifest |
| `MUSE_CODEX_RELEASE_SIGNATURE_URL` | Exact HTTPS URL of its detached signature |
| `MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE` | Local, independently delivered Ed25519 public key |
| `MUSE_CODEX_RELEASE_CURL_CONFIG` | Optional mode-`0600` curl config containing feed authorization |
| `MUSE_CODEX_INSTALL_DIR` | Absolute install directory; defaults to `~/.local/bin` |

Never place feed credentials in URLs. Never store a release signing private key
inside the repository. See [Releasing](RELEASING.md) for operator controls.
