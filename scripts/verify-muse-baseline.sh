#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
baseline_file="$repo_dir/tests/baseline/muse-1.0.3-R2198.1.tsv"
stock_bin=${MUSE_CODEX_MUSE_BIN:-}

if [ -z "$stock_bin" ] || [ ! -x "$stock_bin" ]; then
  echo "Set MUSE_CODEX_MUSE_BIN to the executable Muse 1.0.3-R2198.1 binary." >&2
  exit 2
fi

actual_version=$($stock_bin --version)
if [ "$actual_version" != "Muse Code 1.0.3 (1.0.3-R2198.1)" ]; then
  echo "Unsupported Muse baseline: $actual_version" >&2
  exit 1
fi

probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/muse-codex-baseline.XXXXXX")
cleanup() {
  find "$probe_dir" -depth -delete
}
trap cleanup EXIT HUP INT TERM

while IFS="$(printf '\t')" read -r subcommand expected_exit expected_bytes expected_sha; do
  case "$subcommand" in
    ''|'#'*) continue ;;
  esac
  output_file="$probe_dir/help-$subcommand.txt"
  if [ "$subcommand" = root ]; then
    set +e
    "$stock_bin" --help >"$output_file" 2>&1
    actual_exit=$?
    set -e
  else
    set +e
    "$stock_bin" "$subcommand" --help >"$output_file" 2>&1
    actual_exit=$?
    set -e
  fi
  actual_bytes=$(wc -c <"$output_file" | tr -d ' ')
  actual_sha=$(shasum -a 256 "$output_file" | awk '{print $1}')
  if [ "$actual_exit" != "$expected_exit" ] || \
     [ "$actual_bytes" != "$expected_bytes" ] || \
     [ "$actual_sha" != "$expected_sha" ]; then
    echo "Muse help baseline mismatch for '$subcommand'." >&2
    exit 1
  fi
done <"$baseline_file"

schema_dir="$probe_dir/schema"
"$stock_bin" schema generate-json-schema --out "$schema_dir" >/dev/null
schema_sha=$(shasum -a 256 "$schema_dir/msp.schema.json" | awk '{print $1}')
manifest_sha=$(shasum -a 256 "$schema_dir/manifest.json" | awk '{print $1}')
if [ "$schema_sha" != "f7c77710dbf181b309a3a12060627608dd5c91b1ec0680953ca7381b21181beb" ] || \
   [ "$manifest_sha" != "6d38c445d4cad824c9b225f2a322150391494343aa622f54671b790029c9dc2f" ]; then
  echo "Muse stable MSP schema baseline mismatch." >&2
  exit 1
fi

echo "Muse 1.0.3-R2198.1 baseline: ok"
