#!/usr/bin/env bash
# Build and run one job end to end. Needs Ollama running with llama3.2:1b.
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -e
  cargo build -q -p cf-block-echo-summarize --target wasm32-unknown-unknown
  cargo build -q -p cuttlefishd -p cuttlefish

  sock=/tmp/cuttlefish-tryit.sock
  rm -f "$sock"
  ./target/debug/cuttlefishd examples/summarize.cuttlefish "" "$sock" &
  daemon=$!
  trap "kill $daemon 2>/dev/null || true" EXIT
  until [ -S "$sock" ]; do sleep 0.2; done

  echo
  echo "--- what this daemon can run ---"
  ./target/debug/cuttlefish specs --socket "$sock"

  echo
  echo "--- a job it is allowed to read ---"
  ./target/debug/cuttlefish run --socket "$sock" --spec summarize_docs \
    --input "{\"path\": \"examples/docs/a.txt\"}"
  echo "exit=$?"

  echo
  echo "--- a PDF: read through its text layer ---"
  ./target/debug/cuttlefish run --socket "$sock" --spec summarize_docs \
    --input "{\"path\": \"examples/docs/sample.pdf\"}" || true

  echo
  echo "--- a file it is NOT allowed to read (expect capability_denied, exit 1) ---"
  set +e
  ./target/debug/cuttlefish run --socket "$sock" --spec summarize_docs \
    --input "{\"path\": \"/etc/hosts\"}"
  echo "exit=$?"
'
