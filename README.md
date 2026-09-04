# muse-codex

`muse-codex` keeps the stock Meta Muse Code harness and replaces only its model
transport with an OpenAI Codex/GPT transport. The Muse terminal UI, sessions,
tools, approvals, sandbox, rules, hooks, MCP servers, skills, subagents, and
worktrees continue to be owned by the unmodified Muse executable.

This project is an independent compatibility layer. It is not affiliated with,
endorsed by, or distributed by Meta or OpenAI.

## Important boundary

The stock `muse` executable is a prerequisite. This repository does not contain,
download, modify, package, or redistribute Muse Code. Install Muse from Meta's
official channel and accept Meta's terms separately:

```sh
curl -fsSL https://dev.meta.ai/install.sh | sh
```

The compatibility baseline is Muse `1.0.3-R2198.1`. A newer Muse release may
work, but must pass the black-box compatibility suite before it is declared
supported.

## Architecture at a glance

```text
                       localhost only
user -> muse-codex -> stock muse -> compatibility gateway -> OpenAI
          |               |
          |               +-- TUI, sessions, tools, safety, extensions
          +-- auth commands, process isolation, provider routing
```

The launcher starts a private loopback gateway, gives the Muse child a
per-process bearer token, and points Muse's existing endpoint transport at that
gateway. The gateway implements the Muse model-catalog and Responses streaming
contract and translates it to the pinned Codex client. It never executes a tool;
tool execution and approval remain inside Muse.

See [Architecture](docs/ARCHITECTURE.md) and [Security](docs/SECURITY.md) for the
full boundaries and invariants.

## Requirements

- macOS on Apple silicon (`arm64`)
- Stock Muse Code `1.0.3-R2198.1` installed by Meta, or its exact executable
  selected through `MUSE_CODEX_MUSE_BIN`
- An OpenAI account entitled to the selected authentication mode
- Access to the project's private, immutable release feed
- For source builds: Rust and Cargo matching the repository toolchain

The exact tracked OpenAI Codex `rust-v0.133.0` source used by the transport is
vendored under `vendor/openai-codex`; source builds do not fetch a moving Codex
branch.

ChatGPT subscription authentication and OpenAI API-key billing are distinct
modes. `muse-codex` never silently falls back from one to the other.

## Install a signed private release

Release binaries are not published from a public, mutable URL. Obtain these
values from the release operator through an authenticated channel:

- `MUSE_CODEX_RELEASE_MANIFEST_URL`: exact HTTPS URL of `manifest.json`
- `MUSE_CODEX_RELEASE_SIGNATURE_URL`: exact HTTPS URL of
  `manifest.json.sig`
- `MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE`: local path to the trusted release public
  key, delivered independently of the feed

If the feed needs authentication, put it in a mode-`0600` curl configuration
file and set `MUSE_CODEX_RELEASE_CURL_CONFIG` to its path. This keeps the secret
out of the curl process arguments:

```text
header = "Authorization: Bearer <private-feed-token>"
```

Then run:

```sh
export MUSE_CODEX_RELEASE_MANIFEST_URL='https://private.example/releases/v1/manifest.json'
export MUSE_CODEX_RELEASE_SIGNATURE_URL='https://private.example/releases/v1/manifest.json.sig'
export MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE="$PWD/release-private.pub"
export MUSE_CODEX_RELEASE_CURL_CONFIG="$HOME/.config/muse-codex/release.curlrc"

./scripts/install.sh
```

The default destination directory is `$HOME/.local/bin`. Override it with an
absolute `MUSE_CODEX_INSTALL_DIR`. The installer:

1. verifies that the machine is macOS arm64 and stock Muse is callable;
2. downloads the manifest and detached signature over HTTPS;
3. verifies the manifest with the out-of-band public key;
4. downloads only the signed `macos_arm64` launcher and gateway artifacts;
5. checks both exact sizes and SHA-256 digests;
6. runs the staged gateway self-test and launcher-to-Muse version probe; and
7. installs `muse-codex` and its sibling `muse-codex-gateway`, retaining one
   `.previous` copy of each on update. The gateway is installed first and the
   user-facing launcher last.

It never invokes Meta's installer and never copies the stock Muse binary.

## Use

Authenticate with exactly one mode. Browser login is the default:

```sh
muse-codex login
```

For a terminal that cannot receive the browser callback, use device
authorization:

```sh
muse-codex login --device-auth
```

Or store an OpenAI API key supplied on standard input:

```sh
printf '%s' "$OPENAI_API_KEY" | \
  muse-codex auth set --provider codex --api-key-stdin
```

Credentials are stored only in the operating-system keyring under a dedicated
Muse Codex auth namespace. They are not copied to `~/.codex`, Muse auth, or an
`auth.json` file. A custom OpenAI base URL is allowed only with API-key auth;
subscription login always uses the approved ChatGPT backend.

Then use Muse's normal commands through the wrapper:

```sh
muse-codex
muse-codex exec "Explain the failing tests, then propose a fix"
muse-codex resume
muse-codex logout
```

Arguments unrelated to provider selection pass through to Muse. The public
provider is either omitted or explicitly `--provider codex`; internal `meta`,
`echo`, and unknown providers are rejected. A user `--base-url` or
`OPENAI_BASE_URL` is forwarded only to the gateway as an API-key-authenticated
upstream endpoint. Muse itself always receives the private loopback URL.

## Build and test

```sh
cargo build --workspace --locked
cargo test --workspace --locked
bash tests/scripts/release-tooling-test.sh
```

For a release build on Apple silicon:

```sh
rustup target add aarch64-apple-darwin
cargo build --workspace --release --locked --target aarch64-apple-darwin
```

Release generation is intentionally separate from compilation. See
[Release process](#release-process).

## Release process

Generate a private signing key in protected release infrastructure. Distribute
only its public key to installers:

```sh
ssh-keygen -t ed25519 -N '' -C muse-codex-release -f release-private
chmod 0600 release-private
```

Create and locally verify an immutable release bundle:

```sh
export MUSE_CODEX_RELEASE_VERSION='0.1.0'
export MUSE_CODEX_RELEASE_BASE_URL='https://private.example/releases/0.1.0'
export MUSE_CODEX_RELEASE_SIGNING_KEY_FILE="$PWD/release-private"
export MUSE_CODEX_RELEASE_LAUNCHER_BINARY="$PWD/target/aarch64-apple-darwin/release/muse-codex"
export MUSE_CODEX_RELEASE_GATEWAY_BINARY="$PWD/target/aarch64-apple-darwin/release/muse-codex-gateway"
export MUSE_CODEX_RELEASE_OUTPUT_DIR="$PWD/dist/0.1.0"

./scripts/generate-release-manifest.sh
./scripts/verify-release-manifest.sh \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR/manifest.json" \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR/manifest.json.sig" \
  "$PWD/release-private.pub" \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR"
```

Upload `muse-codex`, `muse-codex-gateway`, `manifest.json`, and
`manifest.json.sig` without renaming them. The feed must use immutable,
access-controlled HTTPS URLs. Never place the private signing key, OpenAI
credentials, Meta credentials, or the stock Muse binary in `dist/`.

## Known compatibility limits

- Meta does not publish the Muse host source. Integration uses the supported
  endpoint and session-protocol surfaces of the shipped executable.
- Transport compatibility does not imply identical model behavior. Muse Spark
  and Muse Code were co-trained; GPT behavior must be assessed separately.
- The ChatGPT backend and pinned Codex Rust crates are private, unstable
  interfaces. Any upstream revision requires a repin and full compatibility
  run.
- Web/search auxiliary routes, new response event types, and new Muse releases
  require explicit compatibility fixtures before support is claimed.
- Stock Muse hooks and MCP servers can execute outside its command sandbox.
  Their existing trust model remains relevant when using this wrapper.

## License and trademarks

The source in this repository is licensed under the Apache License 2.0. Muse,
Muse Code, Meta, OpenAI, ChatGPT, GPT, and Codex are trademarks of their
respective owners. The license for this repository does not grant rights to
redistribute third-party software.
