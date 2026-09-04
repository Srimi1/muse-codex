#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/muse-codex-release-test.XXXXXX")
cleanup() {
  find "$work_dir" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

fail() {
  echo "release-tooling-test: $*" >&2
  exit 1
}

for command_name in ssh-keygen plutil shasum; do
  command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

mkdir -p "$work_dir/input" "$work_dir/release" "$work_dir/install" "$work_dir/fake-bin"

launcher_source=$work_dir/input/muse-codex
gateway_source=$work_dir/input/muse-codex-gateway
stock_muse=$work_dir/input/muse-bin-1.0.3-R2198.1
printf '#!/bin/sh\n[ "${1:-}" = --version ] && exec "$MUSE_CODEX_MUSE_BIN" --version\nprintf "launcher-v1\\n"\n' \
  >"$launcher_source"
printf '#!/bin/sh\n[ "${1:-}" = self-test ] && exit 0\nprintf "gateway-v1\\n"\n' \
  >"$gateway_source"
printf '#!/bin/sh\n[ "${1:-}" = --version ] && printf "Muse Code 1.0.3 (1.0.3-R2198.1)\\n"\n' \
  >"$stock_muse"
chmod 0755 "$launcher_source" "$gateway_source" "$stock_muse"

ssh-keygen -q -t ed25519 -N '' -C muse-codex-release \
  -f "$work_dir/release-private" >/dev/null

MUSE_CODEX_RELEASE_VERSION=0.1.0 \
MUSE_CODEX_RELEASE_BASE_URL=https://release.example.test/muse-codex/0.1.0 \
MUSE_CODEX_RELEASE_SIGNING_KEY_FILE="$work_dir/release-private" \
MUSE_CODEX_RELEASE_LAUNCHER_BINARY="$launcher_source" \
MUSE_CODEX_RELEASE_GATEWAY_BINARY="$gateway_source" \
MUSE_CODEX_RELEASE_OUTPUT_DIR="$work_dir/release" \
MUSE_CODEX_RELEASE_PUBLISHED_AT=2026-09-04T00:00:00Z \
  "$repo_dir/scripts/generate-release-manifest.sh" >/dev/null

"$repo_dir/scripts/verify-release-manifest.sh" \
  "$work_dir/release/manifest.json" \
  "$work_dir/release/manifest.json.sig" \
  "$work_dir/release-private.pub" \
  "$work_dir/release" >/dev/null

json_get() {
  plutil -extract "$1" raw -o - -- "$work_dir/release/manifest.json"
}
assert_manifest_file() {
  field=$1
  expected_name=$2
  expected_source=$3
  [ "$(json_get "$field.filename")" = "$expected_name" ] || \
    fail "$expected_name manifest entry is missing"
  expected_size=$(wc -c <"$expected_source" | tr -d '[:space:]')
  [ "$(json_get "$field.size")" = "$expected_size" ] || \
    fail "$expected_name manifest size is incorrect"
  expected_sha=$(shasum -a 256 "$expected_source" | awk '{print $1}')
  [ "$(json_get "$field.sha256")" = "$expected_sha" ] || \
    fail "$expected_name manifest digest is incorrect"
  [ "$(json_get "$field.url")" = \
    "https://release.example.test/muse-codex/0.1.0/$expected_name" ] || \
    fail "$expected_name manifest URL is incorrect"
}

assert_manifest_file artifacts.macos_arm64.launcher muse-codex "$launcher_source"
assert_manifest_file artifacts.macos_arm64.gateway muse-codex-gateway "$gateway_source"
assert_manifest_file legal.license LICENSE "$repo_dir/LICENSE"
assert_manifest_file legal.third_party_notices THIRD_PARTY_NOTICES.md \
  "$repo_dir/THIRD_PARTY_NOTICES.md"
assert_manifest_file legal.openai_codex_notice NOTICE \
  "$repo_dir/vendor/openai-codex/NOTICE"

cmp "$repo_dir/LICENSE" "$work_dir/release/LICENSE" >/dev/null || \
  fail "release bundle LICENSE differs from the repository source"
cmp "$repo_dir/THIRD_PARTY_NOTICES.md" \
  "$work_dir/release/THIRD_PARTY_NOTICES.md" >/dev/null || \
  fail "release bundle THIRD_PARTY_NOTICES.md differs from the repository source"
cmp "$repo_dir/vendor/openai-codex/NOTICE" \
  "$work_dir/release/NOTICE" >/dev/null || \
  fail "release bundle NOTICE differs from the vendored OpenAI Codex source"
if grep -E 'muse-bin-|"filename"[[:space:]]*:[[:space:]]*"muse"' \
  "$work_dir/release/manifest.json" >/dev/null; then
  fail "release manifest contains a stock Muse artifact"
fi

cp "$work_dir/release/manifest.json" "$work_dir/tampered-manifest.json"
printf ' ' >>"$work_dir/tampered-manifest.json"
if "$repo_dir/scripts/verify-release-manifest.sh" \
  "$work_dir/tampered-manifest.json" \
  "$work_dir/release/manifest.json.sig" \
  "$work_dir/release-private.pub" >/dev/null 2>&1; then
  fail "tampered manifest passed signature verification"
fi

mkdir "$work_dir/tampered-bundle"
for bundled_file in \
  muse-codex muse-codex-gateway LICENSE THIRD_PARTY_NOTICES.md NOTICE
do
  cp "$work_dir/release/$bundled_file" "$work_dir/tampered-bundle/$bundled_file"
done
printf 'tamper' >>"$work_dir/tampered-bundle/muse-codex-gateway"
if "$repo_dir/scripts/verify-release-manifest.sh" \
  "$work_dir/release/manifest.json" \
  "$work_dir/release/manifest.json.sig" \
  "$work_dir/release-private.pub" \
  "$work_dir/tampered-bundle" >/dev/null 2>&1; then
  fail "tampered gateway passed artifact verification"
fi

mkdir "$work_dir/tampered-legal-bundle"
for bundled_file in \
  muse-codex muse-codex-gateway LICENSE THIRD_PARTY_NOTICES.md NOTICE
do
  cp "$work_dir/release/$bundled_file" \
    "$work_dir/tampered-legal-bundle/$bundled_file"
done
printf 'tamper' >>"$work_dir/tampered-legal-bundle/NOTICE"
if "$repo_dir/scripts/verify-release-manifest.sh" \
  "$work_dir/release/manifest.json" \
  "$work_dir/release/manifest.json.sig" \
  "$work_dir/release-private.pub" \
  "$work_dir/tampered-legal-bundle" >/dev/null 2>&1; then
  fail "tampered legal notice passed bundle verification"
fi

mkdir "$work_dir/incomplete-legal-bundle"
for bundled_file in \
  muse-codex muse-codex-gateway LICENSE THIRD_PARTY_NOTICES.md
do
  cp "$work_dir/release/$bundled_file" \
    "$work_dir/incomplete-legal-bundle/$bundled_file"
done
if "$repo_dir/scripts/verify-release-manifest.sh" \
  "$work_dir/release/manifest.json" \
  "$work_dir/release/manifest.json.sig" \
  "$work_dir/release-private.pub" \
  "$work_dir/incomplete-legal-bundle" >/dev/null 2>&1; then
  fail "incomplete legal payload passed bundle verification"
fi

# The installer is exercised without network access. The curl shim reads the
# requested HTTPS URL from curl's stdin config and copies the matching fixture.
cat >"$work_dir/fake-bin/curl" <<'SH'
#!/bin/sh
set -eu
output=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output)
      output=$2
      shift 2
      ;;
    --config|--proto|--proto-redir)
      shift 2
      ;;
    *) shift ;;
  esac
done
[ -n "$output" ] || exit 2
request=$(sed -n 's/^url = "\(.*\)"$/\1/p')
name=${request##*/}
case "$name" in
  manifest.json|manifest.json.sig|muse-codex|muse-codex-gateway)
    cp "$MUSE_CODEX_TEST_RELEASE_DIR/$name" "$output"
    ;;
  *) exit 3 ;;
esac
SH
cat >"$work_dir/fake-bin/uname" <<'SH'
#!/bin/sh
case "${1:-}" in
  -s) echo Darwin ;;
  -m) echo arm64 ;;
  *) echo Darwin ;;
esac
SH
cat >"$work_dir/fake-bin/id" <<'SH'
#!/bin/sh
[ "${1:-}" = -u ] && echo 501
SH
chmod 0755 "$work_dir/fake-bin/curl" "$work_dir/fake-bin/uname" "$work_dir/fake-bin/id"

printf 'old-launcher\n' >"$work_dir/install/muse-codex"
printf 'old-gateway\n' >"$work_dir/install/muse-codex-gateway"
chmod 0755 "$work_dir/install/muse-codex" "$work_dir/install/muse-codex-gateway"

PATH="$work_dir/fake-bin:$PATH" \
MUSE_CODEX_TEST_RELEASE_DIR="$work_dir/release" \
MUSE_CODEX_RELEASE_MANIFEST_URL=https://release.example.test/muse-codex/0.1.0/manifest.json \
MUSE_CODEX_RELEASE_SIGNATURE_URL=https://release.example.test/muse-codex/0.1.0/manifest.json.sig \
MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE="$work_dir/release-private.pub" \
MUSE_CODEX_INSTALL_DIR="$work_dir/install" \
MUSE_CODEX_MUSE_BIN="$stock_muse" \
  "$repo_dir/scripts/install.sh" >/dev/null

cmp "$work_dir/release/muse-codex" "$work_dir/install/muse-codex" >/dev/null || \
  fail "installer did not install the launcher"
cmp "$work_dir/release/muse-codex-gateway" "$work_dir/install/muse-codex-gateway" >/dev/null || \
  fail "installer did not install the gateway"
grep -q '^old-launcher$' "$work_dir/install/muse-codex.previous" || \
  fail "installer did not retain the previous launcher"
grep -q '^old-gateway$' "$work_dir/install/muse-codex-gateway.previous" || \
  fail "installer did not retain the previous gateway"

echo "release tooling: ok"
