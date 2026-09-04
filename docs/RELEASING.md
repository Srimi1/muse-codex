# Releasing

Muse Codex has not published a stable binary release. The tooling in this
repository produces signed bundles for an immutable, access-controlled private
feed. It must not publish or redistribute the stock Muse executable.

## Release prerequisites

- Apple-silicon macOS
- Rust `1.93.0` with the `aarch64-apple-darwin` target
- `ssh-keygen` with SSHSIG support
- A protected Ed25519 signing key stored outside the repository and build output
- An immutable HTTPS destination for release files

The public verification key must be distributed through a channel independent
of the release feed.

## 1. Qualify the source commit

```sh
cargo fmt -p codex-transport -p muse-codex -p muse-codex-gateway -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
bash tests/scripts/release-tooling-test.sh
```

Complete the compatibility gates in [ROADMAP.md](../ROADMAP.md) before calling a
bundle supported. Record the exact Git commit, Rust toolchain, Muse baseline,
and Codex vendor revision in operator release notes.

## 2. Build the artifacts

```sh
rustup target add aarch64-apple-darwin --toolchain 1.93.0
cargo build --workspace --release --locked --target aarch64-apple-darwin
target/aarch64-apple-darwin/release/muse-codex-gateway self-test
```

## 3. Prepare signing material

Generate the release key only in protected release infrastructure, never in the
repository root:

```sh
ssh-keygen -t ed25519 -N '' -C muse-codex-release \
  -f /secure/offline/path/muse-codex-release
chmod 0600 /secure/offline/path/muse-codex-release
```

Back up and rotate the key according to the operator's security policy. A
compromised key requires revoking the corresponding public trust anchor.

## 4. Generate a signed bundle

Choose a new, empty output directory and immutable version URL:

```sh
export MUSE_CODEX_RELEASE_VERSION='0.1.0'
export MUSE_CODEX_RELEASE_BASE_URL='https://private.example/releases/0.1.0'
export MUSE_CODEX_RELEASE_SIGNING_KEY_FILE='/secure/offline/path/muse-codex-release'
export MUSE_CODEX_RELEASE_LAUNCHER_BINARY="$PWD/target/aarch64-apple-darwin/release/muse-codex"
export MUSE_CODEX_RELEASE_GATEWAY_BINARY="$PWD/target/aarch64-apple-darwin/release/muse-codex-gateway"
export MUSE_CODEX_RELEASE_OUTPUT_DIR="$PWD/dist/0.1.0"

./scripts/generate-release-manifest.sh
```

The output contains:

```text
LICENSE
NOTICE
THIRD_PARTY_NOTICES.md
manifest.json
manifest.json.sig
muse-codex
muse-codex-gateway
```

The signed manifest records exact filenames, HTTPS URLs, byte sizes, and SHA-256
digests for both executables and all three legal-notice files.

## 5. Verify independently

```sh
./scripts/verify-release-manifest.sh \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR/manifest.json" \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR/manifest.json.sig" \
  /trusted/path/muse-codex-release.pub \
  "$MUSE_CODEX_RELEASE_OUTPUT_DIR"
```

Inspect the directory manually and confirm that it contains no Muse binary,
credential, private URL secret, signing key, debug artifact, or unrelated file.

## 6. Publish immutably

Upload all seven files without renaming them. Do not overwrite an existing
version path. Verify the remote objects from a clean machine using the
out-of-band public key before distributing the manifest and signature URLs.

The installer's two-binary staging path verifies the signed legal metadata but
installs only `muse-codex` and `muse-codex-gateway`. The full published bundle
retains the accompanying license and notice files.

## Release checklist

- [ ] Source commit and changelog reviewed.
- [ ] CI and local qualification checks pass.
- [ ] Exact Muse and Codex pins recorded.
- [ ] Release build and gateway self-test pass on Apple silicon.
- [ ] Manifest signature, filenames, sizes, and digests verify.
- [ ] License and both third-party notice files are present and signed.
- [ ] Bundle contains no stock Muse artifact or secret.
- [ ] Upload destination is immutable and access-controlled.
- [ ] Public key was delivered independently.
- [ ] Rollback and incident contacts are documented privately.
