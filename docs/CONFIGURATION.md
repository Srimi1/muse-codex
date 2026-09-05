# Configuration

Muse Codex has a deliberately small configuration surface. Defaults favor an
isolated local profile, loopback-only networking, and explicit authentication.

## Authentication modes

| Mode | Command | Storage and billing |
| --- | --- | --- |
| ChatGPT browser | `muse-codex login` | Isolated Codex keyring namespace; ChatGPT account entitlement |
| ChatGPT device | `muse-codex login --device-auth` | Same namespace; device authorization flow |
| OpenAI API key | `muse-codex auth set --provider codex --api-key-stdin` | Keyring; OpenAI API billing |
| Invocation-only API key | Set `OPENAI_API_KEY` for one launch | Sent to the gateway over stdin and removed from the Muse child environment |

Use `muse-codex logout` to remove only the Muse Codex credential entry. The
project does not read or delete stock Codex or Muse credentials.

## Runtime environment

| Variable | Purpose | Rules |
| --- | --- | --- |
| `MUSE_CODEX_MUSE_BIN` | Override stock Muse discovery | Absolute or relative path to an executable reporting exactly `1.0.3-R2198.1` |
| `MUSE_CODEX_GATEWAY_BIN` | Override gateway discovery | Must point to an executable gateway binary |
| `MUSE_CODEX_HOME` | Override isolated application state | Absolute path ending in `muse-codex`; parent must exist; cannot be the home directory or live under `~/.codex` |
| `MUSE_CODEX_GATEWAY_READY_TIMEOUT_MS` | Override gateway startup timeout | Positive integer milliseconds; invalid or zero values use the default |
| `OPENAI_API_KEY` | Supply an invocation-only API key | Consumed by the gateway path and removed from the Muse child environment |
| `OPENAI_BASE_URL` | Override the upstream OpenAI-compatible endpoint | Allowed only with explicit API-key authentication and must use HTTPS |

The default state directory on macOS is:

```text
~/Library/Application Support/muse-codex
```

It is created with private permissions and contains isolated stock-Muse config
and data directories. It is not stock Muse's normal profile.

## Command-line routing

- Omitting `--provider` selects the Muse Codex route.
- `--provider codex` is accepted and removed before Muse is started.
- Other provider values, including `meta` and `echo`, are rejected at the public
  wrapper boundary.
- `--base-url HTTPS_URL` is an API-key-only upstream override. Muse still sees
  the private loopback URL.
- Other arguments pass through to the stock Muse parser.

An explicit Muse `--model` argument remains unchanged. Without one, stock Muse
uses the first picker-visible default in the authenticated upstream catalog.
The launcher atomically seeds Muse's isolated normalized catalog cache so
unknown release dates and output limits remain unknown instead of being
fabricated, while authenticated reasoning-effort choices retain upstream order.
The gateway then translates the selected model's Responses stream.

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
```

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
