# Installation

Muse Codex is experimental and currently distributed as source. It does not
include the proprietary Muse Code executable, and there is no public Muse Codex
binary release.

## Prerequisites

- Apple-silicon Mac (`arm64`)
- Muse Code exactly `1.0.3-R2198.1`, installed and licensed separately
- Rust `1.95.0` and Cargo for a source build
- An OpenAI account eligible for the authentication mode you choose

Install Muse Code through
[Meta's official Muse Code installer](https://dev.meta.ai/install.sh).
Meta's moving installer may provide a newer build than this project supports,
so verify the result:

```console
$ muse --version
Muse Code 1.0.3 (1.0.3-R2198.1)
```

Muse Codex deliberately refuses to run another Muse version. If the exact build
is not available to you through an authorized channel, this version of Muse
Codex cannot start a session.

## Build from source

```sh
git clone https://github.com/Srimi1/muse-codex.git
cd muse-codex
cargo build --workspace --release --locked
```

The repository toolchain file selects Rust `1.95.0`. A locked build uses the
committed dependency graph and pinned Codex source.

Install both executables together:

```sh
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/muse-codex "$HOME/.local/bin/muse-codex"
install -m 0755 target/release/muse-codex-gateway "$HOME/.local/bin/muse-codex-gateway"
```

Ensure `$HOME/.local/bin` is on `PATH`. The launcher discovers the gateway next
to itself first, then on `PATH`.

If Muse is stored somewhere nonstandard, point to the exact executable:

```sh
export MUSE_CODEX_MUSE_BIN='/absolute/path/to/muse-bin-1.0.3-R2198.1'
```

## Verify the build

```sh
MUSE_CODEX_MUSE_BIN="$(command -v muse)" muse-codex self-test
MUSE_CODEX_MUSE_BIN="$(command -v muse)" muse-codex --version
```

The first command validates the installed launcher/gateway protocol pair and
the stock Muse prerequisite without opening Keychain or making a provider
request. The second command should print the exact stock Muse version shown
above.

## Authenticate and run

Choose one authentication mode.

### ChatGPT browser login

```sh
muse-codex login
```

### ChatGPT device authorization

```sh
muse-codex login --device-auth
```

### OpenAI API key

```sh
printf '%s' "$OPENAI_API_KEY" | \
  muse-codex auth set --provider codex --api-key-stdin
```

The stored modes are separate. Authentication failure never changes the billing
mode automatically.

Then run normal Muse commands through the wrapper:

```sh
muse-codex
muse-codex exec "Review this repository"
```

## Install from a signed private feed

The repository includes hardened private-feed tooling for release operators,
but the project does not advertise or operate a public feed. Obtain the
following values from a trusted operator through an authenticated channel:

- `MUSE_CODEX_RELEASE_MANIFEST_URL`
- `MUSE_CODEX_RELEASE_SIGNATURE_URL`
- `MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE`

The public key must be delivered independently of the feed. If the feed needs
authorization, put it in a mode-`0600` curl config file rather than a URL or
shell argument:

```text
header = "Authorization: Bearer <private-feed-token>"
```

Set `MUSE_CODEX_RELEASE_CURL_CONFIG` to that file and run:

```sh
export MUSE_CODEX_RELEASE_MANIFEST_URL='https://private.example/releases/0.1.0/manifest.json'
export MUSE_CODEX_RELEASE_SIGNATURE_URL='https://private.example/releases/0.1.0/manifest.json.sig'
export MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE='/trusted/path/muse-codex-release.pub'
export MUSE_CODEX_RELEASE_CURL_CONFIG='/private/path/release.curlrc'

./scripts/install.sh
```

The installer verifies platform, exact Muse version, manifest signature,
artifact names, sizes, and SHA-256 digests before installing. It stages the
gateway first and launcher last, retains one `.previous` copy during updates,
and never installs the stock Muse binary.

## Troubleshooting

| Symptom | Resolution |
| --- | --- |
| `stock Muse must be exactly ...` | Point `MUSE_CODEX_MUSE_BIN` to the exact supported build. |
| `muse-codex-gateway was not found` | Install both executables together or set `MUSE_CODEX_GATEWAY_BIN`. |
| `no Codex credentials are stored` | Run one of the login or API-key commands above. |
| Gateway startup timeout | Verify the gateway self-test, then review `MUSE_CODEX_GATEWAY_READY_TIMEOUT_MS`. |
| Custom base URL rejected | Use explicit API-key authentication; subscription auth does not allow an override. |

For additional variables and safety rules, see [Configuration](CONFIGURATION.md).
For bugs, follow the [support policy](../SUPPORT.md) and redact sensitive data.
