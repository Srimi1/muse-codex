#!/bin/sh
set -eu

die() {
  echo "muse-codex installer: $*" >&2
  exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
manifest_url=${MUSE_CODEX_RELEASE_MANIFEST_URL:-}
signature_url=${MUSE_CODEX_RELEASE_SIGNATURE_URL:-}
public_key=${MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE:-}
curl_config=${MUSE_CODEX_RELEASE_CURL_CONFIG:-}
install_dir=${MUSE_CODEX_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}
stock_muse=${MUSE_CODEX_MUSE_BIN:-}

[ "$(uname -s)" = Darwin ] || die "only macOS is supported"
case "$(uname -m)" in
  arm64|aarch64) ;;
  *) die "only Apple silicon (arm64) is supported" ;;
esac
[ "$(id -u)" -ne 0 ] || die "do not run this installer as root"

[ -n "$manifest_url" ] || die "MUSE_CODEX_RELEASE_MANIFEST_URL is required"
[ -n "$signature_url" ] || die "MUSE_CODEX_RELEASE_SIGNATURE_URL is required"
[ -n "$public_key" ] || die "MUSE_CODEX_RELEASE_PUBLIC_KEY_FILE is required"
case "$manifest_url" in https://*) ;; *) die "manifest URL must use HTTPS" ;; esac
case "$signature_url" in https://*) ;; *) die "signature URL must use HTTPS" ;; esac
case "$manifest_url$signature_url" in
  *\"*|*\\*|*' '*|*"$(printf '\t')"*) die "release URL is malformed" ;;
esac
if printf '%s' "$manifest_url$signature_url" | LC_ALL=C grep '[[:cntrl:]]' >/dev/null; then
  die "release URL contains control characters"
fi
[ -f "$public_key" ] && [ ! -L "$public_key" ] || \
  die "release public key must be a regular, non-symlink file"

case "$install_dir" in
  /*) ;;
  *) die "MUSE_CODEX_INSTALL_DIR must be an absolute path" ;;
esac
[ "$install_dir" != / ] && [ "$install_dir" != "$HOME" ] || \
  die "refusing unsafe install directory: $install_dir"

for command_name in curl ssh-keygen plutil shasum; do
  command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done

if [ -n "$curl_config" ]; then
  [ -f "$curl_config" ] && [ ! -L "$curl_config" ] || \
    die "release curl config must be a regular, non-symlink file"
  if stat -f '%Lp' "$curl_config" >/dev/null 2>&1; then
    curl_mode=$(stat -f '%Lp' "$curl_config")
  else
    curl_mode=$(stat -c '%a' "$curl_config")
  fi
  [ "$curl_mode" = 600 ] || die "release curl config permissions must be 0600"
fi

if [ -z "$stock_muse" ]; then
  stock_muse=$(command -v muse || true)
fi
[ -n "$stock_muse" ] && [ -x "$stock_muse" ] || \
  die "stock Muse is required; install it separately or set MUSE_CODEX_MUSE_BIN"
stock_version=$(MUSE_NO_AUTO_UPDATE=1 "$stock_muse" --version 2>/dev/null || true)
[ "$stock_version" = "Muse Code 1.0.3 (1.0.3-R2198.1)" ] || \
  die "stock Muse must be exactly 1.0.3-R2198.1 (found: ${stock_version:-unknown})"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/muse-codex-install.XXXXXX")
launcher_stage=
gateway_stage=
committed=0
launcher_backed_up=0
gateway_backed_up=0
launcher_installed=0
gateway_installed=0

cleanup() {
  if [ "$committed" -eq 0 ]; then
    if [ "$launcher_installed" -eq 1 ]; then rm -f -- "$install_dir/muse-codex"; fi
    if [ "$gateway_installed" -eq 1 ]; then rm -f -- "$install_dir/muse-codex-gateway"; fi
    if [ "$launcher_backed_up" -eq 1 ]; then
      mv -- "$install_dir/muse-codex.previous" "$install_dir/muse-codex"
    fi
    if [ "$gateway_backed_up" -eq 1 ]; then
      mv -- "$install_dir/muse-codex-gateway.previous" "$install_dir/muse-codex-gateway"
    fi
  fi
  [ -z "$launcher_stage" ] || [ ! -e "$launcher_stage" ] || rm -f -- "$launcher_stage"
  [ -z "$gateway_stage" ] || [ ! -e "$gateway_stage" ] || rm -f -- "$gateway_stage"
  find "$work_dir" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

download_https() {
  url=$1
  destination=$2
  case "$url" in https://*) ;; *) die "artifact URL must use HTTPS" ;; esac
  case "$url" in *\"*|*\\*|*' '*|*"$(printf '\t')"*) die "artifact URL is malformed" ;; esac
  if printf '%s' "$url" | LC_ALL=C grep '[[:cntrl:]]' >/dev/null; then
    die "artifact URL contains control characters"
  fi
  if [ -n "$curl_config" ]; then
    printf 'url = "%s"\n' "$url" | \
      curl -q --fail --silent --show-error --proto '=https' \
        --config "$curl_config" --config - --output "$destination"
  else
    printf 'url = "%s"\n' "$url" | \
      curl -q --fail --silent --show-error --proto '=https' \
        --config - --output "$destination"
  fi
}

manifest=$work_dir/manifest.json
signature=$work_dir/manifest.json.sig
download_https "$manifest_url" "$manifest"
download_https "$signature_url" "$signature"
"$script_dir/verify-release-manifest.sh" "$manifest" "$signature" "$public_key" >/dev/null

json_get() {
  plutil -extract "$1" raw -o - -- "$manifest" 2>/dev/null || \
    die "signed manifest field '$1' is missing or invalid"
}

launcher_url=$(json_get artifacts.macos_arm64.launcher.url)
gateway_url=$(json_get artifacts.macos_arm64.gateway.url)
download_https "$launcher_url" "$work_dir/muse-codex"
download_https "$gateway_url" "$work_dir/muse-codex-gateway"
chmod 0755 "$work_dir/muse-codex" "$work_dir/muse-codex-gateway"
"$script_dir/verify-release-manifest.sh" \
  "$manifest" "$signature" "$public_key" "$work_dir" >/dev/null

# Exercise the downloaded pair before changing the installation. This checks
# the gateway's credential-free internals, launcher/readiness wire contract,
# and separately installed stock Muse compatibility in one probe.
MUSE_CODEX_MUSE_BIN="$stock_muse" \
MUSE_CODEX_GATEWAY_BIN="$work_dir/muse-codex-gateway" \
  "$work_dir/muse-codex" self-test >/dev/null || \
  die "downloaded muse-codex pair failed its compatibility self-test"

mkdir -p -- "$install_dir"
for existing in \
  "$install_dir/muse-codex" \
  "$install_dir/muse-codex-gateway" \
  "$install_dir/muse-codex.previous" \
  "$install_dir/muse-codex-gateway.previous"
do
  if [ -e "$existing" ] || [ -L "$existing" ]; then
    [ -f "$existing" ] && [ ! -L "$existing" ] || \
      die "refusing to replace non-regular path: $existing"
  fi
done

launcher_stage=$(mktemp "$install_dir/.muse-codex.new.XXXXXX")
gateway_stage=$(mktemp "$install_dir/.muse-codex-gateway.new.XXXXXX")
cp -- "$work_dir/muse-codex" "$launcher_stage"
cp -- "$work_dir/muse-codex-gateway" "$gateway_stage"
chmod 0755 "$launcher_stage" "$gateway_stage"

if [ -e "$install_dir/muse-codex.previous" ]; then
  rm -f -- "$install_dir/muse-codex.previous"
fi
if [ -e "$install_dir/muse-codex-gateway.previous" ]; then
  rm -f -- "$install_dir/muse-codex-gateway.previous"
fi
if [ -e "$install_dir/muse-codex" ]; then
  mv -- "$install_dir/muse-codex" "$install_dir/muse-codex.previous"
  launcher_backed_up=1
fi
if [ -e "$install_dir/muse-codex-gateway" ]; then
  mv -- "$install_dir/muse-codex-gateway" "$install_dir/muse-codex-gateway.previous"
  gateway_backed_up=1
fi

mv -- "$gateway_stage" "$install_dir/muse-codex-gateway"
gateway_stage=
gateway_installed=1
mv -- "$launcher_stage" "$install_dir/muse-codex"
launcher_stage=
launcher_installed=1
committed=1

echo "Installed muse-codex and muse-codex-gateway in $install_dir"
