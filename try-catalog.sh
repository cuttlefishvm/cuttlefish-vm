#!/usr/bin/env bash
# Exercise the block catalog end to end. No daemon, no Ollama — the catalog
# is a purely local filesystem store.
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -e
  cargo build -q -p cf-block-echo-summarize --target wasm32-unknown-unknown
  cargo build -q -p cuttlefish

  export CUTTLEFISH_HOME="$(mktemp -d)"
  trap "rm -rf \"$CUTTLEFISH_HOME\"" EXIT
  cf=./target/debug/cuttlefish

  echo
  echo "--- catalog a real block ---"
  "$cf" catalog add echo-summarize@1 \
    target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm

  echo
  echo "--- list ---"
  "$cf" catalog list

  echo
  echo "--- show ---"
  "$cf" catalog show echo-summarize@1

  echo
  echo "--- re-adding the same name@version (expect AlreadyExists, exit 1) ---"
  set +e
  "$cf" catalog add echo-summarize@1 \
    target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm
  echo "exit=$?"
  set -e

  echo
  echo "--- a typo lookup (expect NotFound with a did-you-mean suggestion, exit 1) ---"
  set +e
  "$cf" catalog show ech-summarize@1
  echo "exit=$?"
  set -e

  echo
  echo "--- dropping the @version (expect InvalidNameVersion, exit 1) ---"
  set +e
  "$cf" catalog add echo-summarize \
    target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm
  echo "exit=$?"
  set -e

  echo
  echo "--- rm (index-only; the blob stays on disk) ---"
  "$cf" catalog rm echo-summarize@1
  "$cf" catalog list

  echo
  echo "--- re-adding the removed version with the SAME bytes (expect success) ---"
  "$cf" catalog add echo-summarize@1 \
    target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm

  echo
  echo "--- rm, then re-add with DIFFERENT bytes (expect RetiredWithDifferentContent, exit 1) ---"
  "$cf" catalog rm echo-summarize@1
  printf "\x00asm\x01\x00\x00\x00" > "$CUTTLEFISH_HOME/other.wasm"
  set +e
  "$cf" catalog add echo-summarize@1 "$CUTTLEFISH_HOME/other.wasm"
  echo "exit=$?"
  set -e
'
