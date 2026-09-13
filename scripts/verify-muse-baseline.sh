#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
stock_bin=${MUSE_CODEX_MUSE_BIN:-}

if [ -z "$stock_bin" ] || [ ! -x "$stock_bin" ]; then
  echo "Set MUSE_CODEX_MUSE_BIN to an executable supported Muse binary (1.0.3-R2198.1 or 1.2.1-R2847.1)." >&2
  exit 2
fi

actual_version=$($stock_bin --version)
case "$actual_version" in
  "Muse Code 1.0.3 (1.0.3-R2198.1)")
    baseline_file="$repo_dir/tests/baseline/muse-1.0.3-R2198.1.tsv"
    expected_schema_sha=f7c77710dbf181b309a3a12060627608dd5c91b1ec0680953ca7381b21181beb
    expected_manifest_sha=6d38c445d4cad824c9b225f2a322150391494343aa622f54671b790029c9dc2f
    ;;
  "Muse Code 1.2.1 (1.2.1-R2847.1)")
    baseline_file="$repo_dir/tests/baseline/muse-1.2.1-R2847.1.tsv"
    expected_schema_sha=8db08d48551788a3fe6deeb0188723fc4d7d529b464fda66f71bfbf3babee066
    expected_manifest_sha=916c5b62d0e39027dc926319dd96519d9f4832cc4aeb7999c29ac47f709abc48
    ;;
  *)
    echo "Unsupported Muse baseline: $actual_version" >&2
    exit 1
    ;;
esac

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
if [ "$schema_sha" != "$expected_schema_sha" ] || \
   [ "$manifest_sha" != "$expected_manifest_sha" ]; then
  echo "Muse stable MSP schema baseline mismatch." >&2
  exit 1
fi

echo "Muse baseline $actual_version: ok"
