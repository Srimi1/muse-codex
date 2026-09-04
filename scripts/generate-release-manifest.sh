#!/bin/sh
set -eu

die() {
  echo "generate-release-manifest: $*" >&2
  exit 1
}

version=${MUSE_CODEX_RELEASE_VERSION:-}
base_url=${MUSE_CODEX_RELEASE_BASE_URL:-}
signing_key=${MUSE_CODEX_RELEASE_SIGNING_KEY_FILE:-}
launcher_source=${MUSE_CODEX_RELEASE_LAUNCHER_BINARY:-}
gateway_source=${MUSE_CODEX_RELEASE_GATEWAY_BINARY:-}
output_dir=${MUSE_CODEX_RELEASE_OUTPUT_DIR:-}
published_at=${MUSE_CODEX_RELEASE_PUBLISHED_AT:-$(date -u '+%Y-%m-%dT%H:%M:%SZ')}

[ -n "$version" ] || die "MUSE_CODEX_RELEASE_VERSION is required"
[ -n "$base_url" ] || die "MUSE_CODEX_RELEASE_BASE_URL is required"
[ -n "$signing_key" ] || die "MUSE_CODEX_RELEASE_SIGNING_KEY_FILE is required"
[ -n "$launcher_source" ] || die "MUSE_CODEX_RELEASE_LAUNCHER_BINARY is required"
[ -n "$gateway_source" ] || die "MUSE_CODEX_RELEASE_GATEWAY_BINARY is required"
[ -n "$output_dir" ] || die "MUSE_CODEX_RELEASE_OUTPUT_DIR is required"

case "$version" in
  *[!A-Za-z0-9._-]*|'') die "release version contains unsafe characters" ;;
esac
case "$base_url" in
  https://*) ;;
  *) die "release base URL must use HTTPS" ;;
esac
case "$base_url" in
  *\"*|*\\*|*' '*|*"$(printf '\t')"*)
    die "release base URL contains characters unsafe for the manifest"
    ;;
esac
if printf '%s' "$base_url" | LC_ALL=C grep '[[:cntrl:]]' >/dev/null; then
  die "release base URL contains control characters"
fi
case "$published_at" in
  *[!0-9T:Z+.-]*|'') die "publication timestamp is malformed" ;;
esac

[ -f "$signing_key" ] || die "signing key is not a regular file"
[ -f "$launcher_source" ] && [ -x "$launcher_source" ] || \
  die "launcher binary is not an executable file"
[ -f "$gateway_source" ] && [ -x "$gateway_source" ] || \
  die "gateway binary is not an executable file"

command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen with SSHSIG support is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

mkdir -p -- "$output_dir"
if [ -n "$(ls -A "$output_dir")" ]; then
  die "output directory must be empty: $output_dir"
fi

launcher_name=muse-codex
gateway_name=muse-codex-gateway
launcher_output=$output_dir/$launcher_name
gateway_output=$output_dir/$gateway_name
cp -- "$launcher_source" "$launcher_output"
cp -- "$gateway_source" "$gateway_output"
chmod 0755 "$launcher_output" "$gateway_output"

file_size() {
  wc -c <"$1" | tr -d '[:space:]'
}

file_sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

launcher_size=$(file_size "$launcher_output")
gateway_size=$(file_size "$gateway_output")
launcher_sha=$(file_sha256 "$launcher_output")
gateway_sha=$(file_sha256 "$gateway_output")
launcher_url=${base_url%/}/$launcher_name
gateway_url=${base_url%/}/$gateway_name

manifest_tmp=$(mktemp "$output_dir/.manifest.json.XXXXXX")
signature_tmp=$(mktemp "$output_dir/.manifest.json.sig.XXXXXX")
cleanup() {
  [ ! -e "$manifest_tmp" ] || rm -f -- "$manifest_tmp"
  [ ! -e "$signature_tmp" ] || rm -f -- "$signature_tmp"
}
trap cleanup EXIT HUP INT TERM

{
  printf '{\n'
  printf '  "schema_version": 1,\n'
  printf '  "product": "muse-codex",\n'
  printf '  "version": "%s",\n' "$version"
  printf '  "minimum_tested_muse": "1.0.3-R2198.1",\n'
  printf '  "published_at": "%s",\n' "$published_at"
  printf '  "artifacts": {\n'
  printf '    "macos_arm64": {\n'
  printf '      "launcher": {"filename": "%s", "url": "%s", "size": %s, "sha256": "%s"},\n' \
    "$launcher_name" "$launcher_url" "$launcher_size" "$launcher_sha"
  printf '      "gateway": {"filename": "%s", "url": "%s", "size": %s, "sha256": "%s"}\n' \
    "$gateway_name" "$gateway_url" "$gateway_size" "$gateway_sha"
  printf '    }\n'
  printf '  }\n'
  printf '}\n'
} >"$manifest_tmp"

rm -f -- "$signature_tmp"
ssh-keygen -Y sign -f "$signing_key" -n muse-codex-release \
  "$manifest_tmp" >/dev/null
mv -- "$manifest_tmp.sig" "$signature_tmp"
mv -- "$manifest_tmp" "$output_dir/manifest.json"
mv -- "$signature_tmp" "$output_dir/manifest.json.sig"
trap - EXIT HUP INT TERM

echo "Created signed two-binary release bundle in $output_dir"
