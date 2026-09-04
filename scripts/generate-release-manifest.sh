#!/bin/sh
set -eu

die() {
  echo "generate-release-manifest: $*" >&2
  exit 1
}

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=${MUSE_CODEX_RELEASE_VERSION:-}
base_url=${MUSE_CODEX_RELEASE_BASE_URL:-}
signing_key=${MUSE_CODEX_RELEASE_SIGNING_KEY_FILE:-}
launcher_source=${MUSE_CODEX_RELEASE_LAUNCHER_BINARY:-}
gateway_source=${MUSE_CODEX_RELEASE_GATEWAY_BINARY:-}
license_source=$repo_dir/LICENSE
third_party_notices_source=$repo_dir/THIRD_PARTY_NOTICES.md
openai_codex_notice_source=$repo_dir/vendor/openai-codex/NOTICE
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
[ -f "$license_source" ] || die "root LICENSE is not a regular file"
[ -f "$third_party_notices_source" ] || \
  die "root THIRD_PARTY_NOTICES.md is not a regular file"
[ -f "$openai_codex_notice_source" ] || \
  die "vendor/openai-codex/NOTICE is not a regular file"

command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen with SSHSIG support is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

mkdir -p -- "$output_dir"
if [ -n "$(ls -A "$output_dir")" ]; then
  die "output directory must be empty: $output_dir"
fi

launcher_name=muse-codex
gateway_name=muse-codex-gateway
license_name=LICENSE
third_party_notices_name=THIRD_PARTY_NOTICES.md
openai_codex_notice_name=NOTICE
launcher_output=$output_dir/$launcher_name
gateway_output=$output_dir/$gateway_name
license_output=$output_dir/$license_name
third_party_notices_output=$output_dir/$third_party_notices_name
openai_codex_notice_output=$output_dir/$openai_codex_notice_name
cp -- "$launcher_source" "$launcher_output"
cp -- "$gateway_source" "$gateway_output"
cp -- "$license_source" "$license_output"
cp -- "$third_party_notices_source" "$third_party_notices_output"
cp -- "$openai_codex_notice_source" "$openai_codex_notice_output"
chmod 0755 "$launcher_output" "$gateway_output"
chmod 0644 \
  "$license_output" \
  "$third_party_notices_output" \
  "$openai_codex_notice_output"

file_size() {
  wc -c <"$1" | tr -d '[:space:]'
}

file_sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

launcher_size=$(file_size "$launcher_output")
gateway_size=$(file_size "$gateway_output")
license_size=$(file_size "$license_output")
third_party_notices_size=$(file_size "$third_party_notices_output")
openai_codex_notice_size=$(file_size "$openai_codex_notice_output")
launcher_sha=$(file_sha256 "$launcher_output")
gateway_sha=$(file_sha256 "$gateway_output")
license_sha=$(file_sha256 "$license_output")
third_party_notices_sha=$(file_sha256 "$third_party_notices_output")
openai_codex_notice_sha=$(file_sha256 "$openai_codex_notice_output")
launcher_url=${base_url%/}/$launcher_name
gateway_url=${base_url%/}/$gateway_name
license_url=${base_url%/}/$license_name
third_party_notices_url=${base_url%/}/$third_party_notices_name
openai_codex_notice_url=${base_url%/}/$openai_codex_notice_name

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
  printf '  },\n'
  printf '  "legal": {\n'
  printf '    "license": {"filename": "%s", "url": "%s", "size": %s, "sha256": "%s"},\n' \
    "$license_name" "$license_url" "$license_size" "$license_sha"
  printf '    "third_party_notices": {"filename": "%s", "url": "%s", "size": %s, "sha256": "%s"},\n' \
    "$third_party_notices_name" "$third_party_notices_url" \
    "$third_party_notices_size" "$third_party_notices_sha"
  printf '    "openai_codex_notice": {"filename": "%s", "url": "%s", "size": %s, "sha256": "%s"}\n' \
    "$openai_codex_notice_name" "$openai_codex_notice_url" \
    "$openai_codex_notice_size" "$openai_codex_notice_sha"
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

echo "Created signed release bundle with binaries and legal notices in $output_dir"
