#!/usr/bin/env bash
# Close the loop: a campaign escalates the item no ladder can rescue, the
# escalation is drained back into a manifest, and a SECOND spec runs that
# manifest to success.
#
# The second spec is the whole point. Anything can write a JSONL file; what
# this proves is that the file cuttlefish hands back is byte-for-byte
# consumable by `over`, with no massaging. That is the reason the manifest
# holds raw inputs and nothing else — a provenance field would change each
# value's shape and fail the consuming block's declared input type.
#
# Not asserted here: rows skipped as unrecoverable (needs a hand-built
# old-schema ledger, which belongs at the Rust level in
# crates/cuttlefishd/tests/api.rs).
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -eo pipefail
  cargo build -q -p cuttlefish -p cuttlefishd

  work="$(mktemp -d)"
  daemon_pid=""
  trap '"'"'[ -n "$daemon_pid" ] && kill -9 "$daemon_pid" 2>/dev/null; rm -rf "$work"'"'"' EXIT

  export CUTTLEFISH_HOME="$work/home"
  export CUTTLEFISH_JOBS_HOME="$work/jobs"
  mkdir -p "$work/corpus" "$work/schemas" "$work/blocks"

  cf=./target/debug/cuttlefish
  cfd=./target/debug/cuttlefishd

  cat > "$work/blocks/grade.rhai" <<RHAI
//! signature: json -> json
#{ score: input.n }
RHAI
  "$cf" catalog add grade@1 "$work/blocks/grade.rhai"

  # Strict: only 5 or better is acceptable. The item scoring 2 can never
  # pass, no matter how many times it is retried.
  cat > "$work/schemas/strict.json" <<JSON
{"type":"object","required":["score"],"properties":{"score":{"type":"integer","minimum":5}}}
JSON
  # Loose: what the planner decides on *after* seeing the escalation.
  cat > "$work/schemas/loose.json" <<JSON
{"type":"object","required":["score"],"properties":{"score":{"type":"integer","minimum":1}}}
JSON

  cat > "$work/corpus/manifest.jsonl" <<JSONL
{"n": 7}
{"n": 2}
{"n": 9}
JSONL

  spec_body() {
    cat <<SPEC
spec $1 = {
  description = "Use when each item of a manifest must be graded against a threshold.";
  model = Stub "unused";
  data_policy = Local_only;
  capabilities = [ Read "./corpus", Read "./schemas", Read "$work" ];
  nodes = {
    analyze = {
      block   = "grade@1";
      over    = "$2";
      accept  = [ Schema "./schemas/$3.json" ];
      on_fail = [ retry 1, escalate ];
    };
  };
}
SPEC
  }

  spec_body graded_strict "./corpus/manifest.jsonl" strict > "$work/first.cuttlefish"

  hash=$(pwd -P | shasum -a 256 | cut -c1-16)
  endpoint="${TMPDIR:-/tmp}/cuttlefish-drain-$hash.sock"

  start_daemon() {
    rm -f "$endpoint"
    "$cfd" "$1" "$endpoint" > "$work/daemon.log" 2>&1 &
    daemon_pid=$!
    for _ in $(seq 1 100); do
      "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && return 0
      sleep 0.1
    done
    echo "daemon never came up; log:"; cat "$work/daemon.log"; exit 1
  }

  # --- run 1: one item escalates --------------------------------------
  start_daemon "$work/first.cuttlefish"
  echo "--- run 1: strict threshold, one item cannot pass ---"
  out=$("$cf" run --endpoint "$endpoint" --spec graded_strict --input "{}" 2>&1) || {
    echo "$out"; cat "$work/daemon.log"; exit 1; }
  failed=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"failed\"])")
  [ "$failed" = "1" ] || { echo "FAIL: expected 1 failure, got $failed"; exit 1; }

  # --- drain ----------------------------------------------------------
  echo
  echo "--- draining the escalation back into a manifest ---"
  "$cf" escalations --endpoint "$endpoint" --manifest "$work/retry.jsonl"
  echo "--- retry.jsonl ---"
  cat "$work/retry.jsonl"

  # The manifest must hold the ORIGINAL input, not the failing output.
  grep -q "\"n\":2\|\"n\": 2" "$work/retry.jsonl" || {
    echo "FAIL: drained manifest does not carry the original input"; exit 1; }
  [ "$(wc -l < "$work/retry.jsonl")" -eq 1 ] || {
    echo "FAIL: expected exactly one drained line"; exit 1; }

  # Draining is idempotent: the row is marked, so a second drain finds
  # nothing rather than handing the same work out twice.
  second=$("$cf" escalations --endpoint "$endpoint" --manifest "$work/again.jsonl" 2>&1)
  echo "$second" | grep -q "no escalations to drain" || {
    echo "FAIL: a drained escalation came back on the next drain: $second"; exit 1; }
  # ...but the history survives.
  "$cf" escalations --endpoint "$endpoint" --all | grep -q "drained" || {
    echo "FAIL: --all lost the drained record"; exit 1; }

  "$cf" shutdown --endpoint "$endpoint" >/dev/null
  wait "$daemon_pid" 2>/dev/null || true

  # --- run 2: the drained manifest, with a threshold that fits --------
  spec_body graded_loose "$work/retry.jsonl" loose > "$work/second.cuttlefish"
  start_daemon "$work/second.cuttlefish"

  echo
  echo "--- run 2: the drained manifest, loosened threshold ---"
  out2=$("$cf" run --endpoint "$endpoint" --spec graded_loose --input "{}" 2>&1) || {
    echo "$out2"; cat "$work/daemon.log"; exit 1; }
  echo "$out2"
  ok=$(echo "$out2" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"succeeded\"])")
  bad=$(echo "$out2" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"failed\"])")
  [ "$ok" = "1" ]  || { echo "FAIL: drained item did not succeed on re-run (succeeded=$ok)"; exit 1; }
  [ "$bad" = "0" ] || { echo "FAIL: re-run still had $bad failure(s)"; exit 1; }

  "$cf" shutdown --endpoint "$endpoint" >/dev/null
  echo
  echo "OK: escalated item drained to a manifest and re-run to success by a second spec"
'
