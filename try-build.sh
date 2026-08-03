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

  export CUTTLEFISH_HOME="$(mktemp -d)"
  trap "rm -rf \"$CUTTLEFISH_HOME\"" EXIT
  cf=./target/debug/cuttlefish
  wasm=target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm

  echo
  echo "--- build a single-block spec by direct path ---"
  "$cf" build examples/summarize.cuttlefish -o /tmp/summarize.cfbundle
  ls -la /tmp/summarize.cfbundle

  echo
  echo "--- catalog it, then build a spec that references it by name ---"
  "$cf" catalog add echo-summarize@1 "$wasm"
  tmp_spec="$(mktemp -d)/by-name.cuttlefish"
  cat > "$tmp_spec" <<EOF
spec by_name = {
  description = "build-by-catalog-name smoke test";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [];
  block = "echo-summarize@1";
}
EOF
  "$cf" build "$tmp_spec" -o /tmp/by-name.cfbundle
  ls -la /tmp/by-name.cfbundle

  echo
  echo "--- rebuild the same spec, confirm byte-identical output ---"
  "$cf" build "$tmp_spec" -o /tmp/by-name-2.cfbundle
  cmp /tmp/by-name.cfbundle /tmp/by-name-2.cfbundle && echo "identical: ok"

  rm -f /tmp/summarize.cfbundle /tmp/by-name.cfbundle /tmp/by-name-2.cfbundle
'
