#!/usr/bin/env bash
# Exercise `cuttlefish build` end to end: a real spec, a real compiled
# block, a real catalog entry used as a pipeline stage, and byte-identical
# rebuilds.
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -e
  cargo build -q -p cf-block-echo-summarize --target wasm32-unknown-unknown
  cargo build -q -p cuttlefish

  # Everything this run creates — the catalog, the throwaway spec, and every
  # built bundle — lives under one work dir so a single trap cleans it all
  # up, including on a failure partway through (set -e would otherwise skip
  # a trailing `rm -f` and leave multi-megabyte bundles behind in /tmp).
  work="$(mktemp -d)"
  trap "rm -rf \"$work\"" EXIT
  export CUTTLEFISH_HOME="$work/home"
  cf=./target/debug/cuttlefish
  wasm=target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm

  echo
  echo "--- build a single-block spec by direct path ---"
  "$cf" build examples/summarize.cuttlefish -o "$work/summarize.cfbundle"
  ls -la "$work/summarize.cfbundle"

  echo
  echo "--- catalog it, then build a spec that references it by name ---"
  "$cf" catalog add echo-summarize@1 "$wasm"
  tmp_spec="$work/by-name.cuttlefish"
  cat > "$tmp_spec" <<EOF
spec by_name = {
  description = "build-by-catalog-name smoke test";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [];
  block = "echo-summarize@1";
}
EOF
  "$cf" build "$tmp_spec" -o "$work/by-name.cfbundle"
  ls -la "$work/by-name.cfbundle"

  echo
  echo "--- rebuild the same spec, confirm byte-identical output ---"
  "$cf" build "$tmp_spec" -o "$work/by-name-2.cfbundle"
  cmp "$work/by-name.cfbundle" "$work/by-name-2.cfbundle" && echo "identical: ok"
'
