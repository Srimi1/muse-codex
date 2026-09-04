#!/bin/sh
set -eu

die() {
  echo "verify-release-manifest: $*" >&2
  exit 1
}

[ "$#" -eq 3 ] || [ "$#" -eq 4 ] || {
  echo "usage: $0 MANIFEST SIGNATURE PUBLIC_KEY [BUNDLE_DIRECTORY]" >&2
  exit 2
}

manifest=$1
signature=$2
public_key=$3
bundle_dir=${4:-}

[ -f "$manifest" ] || die "manifest is not a regular file"
[ -f "$signature" ] || die "signature is not a regular file"
[ -f "$public_key" ] || die "public key is not a regular file"
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen with SSHSIG support is required"
command -v plutil >/dev/null 2>&1 || die "plutil is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

allowed_signers=$(mktemp "${TMPDIR:-/tmp}/muse-codex-allowed-signers.XXXXXX")
cleanup() {
  rm -f -- "$allowed_signers"
}
trap cleanup EXIT HUP INT TERM
public_material=$(awk 'NR == 1 && $1 == "ssh-ed25519" { print $1 " " $2 }' "$public_key")
[ -n "$public_material" ] || die "release public key is not an OpenSSH Ed25519 public key"
printf 'muse-codex %s\n' "$public_material" >"$allowed_signers"
chmod 0600 "$allowed_signers"
ssh-keygen -Y verify -f "$allowed_signers" -I muse-codex \
  -n muse-codex-release -s "$signature" <"$manifest" \
  >/dev/null 2>&1 || die "manifest Ed25519 signature verification failed"
plutil -convert json -o /dev/null -- "$manifest" >/dev/null || \
  die "manifest is not valid JSON"

json_get() {
  plutil -extract "$1" raw -o - -- "$manifest" 2>/dev/null || \
    die "manifest field '$1' is missing or invalid"
}

[ "$(json_get schema_version)" = 1 ] || die "unsupported manifest schema"
[ "$(json_get product)" = muse-codex ] || die "manifest product is not muse-codex"
[ "$(json_get minimum_tested_muse)" = 1.0.3-R2198.1 ] || \
  die "manifest Muse baseline is unsupported"

version=$(json_get version)
case "$version" in
  *[!A-Za-z0-9._-]*|'') die "manifest version is malformed" ;;
esac
published_at=$(json_get published_at)
case "$published_at" in
  *[!0-9T:Z+.-]*|'') die "manifest publication timestamp is malformed" ;;
esac

validate_artifact() {
  role=$1
  expected_name=$2
  prefix=artifacts.macos_arm64.$role
  filename=$(json_get "$prefix.filename")
  url=$(json_get "$prefix.url")
  size=$(json_get "$prefix.size")
  sha256=$(json_get "$prefix.sha256")

  [ "$filename" = "$expected_name" ] || \
    die "$role artifact filename is not '$expected_name'"
  case "$url" in
    https://*) ;;
    *) die "$role artifact URL must use HTTPS" ;;
  esac
  case "$url" in
    *\"*|*\\*|*' '*|*"$(printf '\t')"*) die "$role artifact URL is malformed" ;;
  esac
  if printf '%s' "$url" | LC_ALL=C grep '[[:cntrl:]]' >/dev/null; then
    die "$role artifact URL contains control characters"
  fi
  case "$size" in
    *[!0-9]*|'') die "$role artifact size is malformed" ;;
  esac
  [ "$size" -gt 0 ] || die "$role artifact size must be positive"
  case "$sha256" in
    *[!0-9a-f]*|'') die "$role artifact SHA-256 is malformed" ;;
  esac
  [ "${#sha256}" -eq 64 ] || die "$role artifact SHA-256 is malformed"

  if [ -n "$bundle_dir" ]; then
    artifact=$bundle_dir/$filename
    [ -f "$artifact" ] || die "$role artifact is missing from bundle"
    actual_size=$(wc -c <"$artifact" | tr -d '[:space:]')
    [ "$actual_size" = "$size" ] || die "$role artifact size does not match manifest"
    actual_sha=$(shasum -a 256 "$artifact" | awk '{print $1}')
    [ "$actual_sha" = "$sha256" ] || die "$role artifact digest does not match manifest"
  fi
}

validate_artifact launcher muse-codex
validate_artifact gateway muse-codex-gateway

if grep -E 'muse-bin-|"filename"[[:space:]]*:[[:space:]]*"muse"' "$manifest" >/dev/null; then
  die "manifest attempts to include a stock Muse artifact"
fi

echo "muse-codex release manifest: ok"
