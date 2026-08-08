#!/usr/bin/env bash
# Exercise campaign fan-out end to end: catalog a map block and a reduce
# block, write a manifest with one deliberately-bad item, run the job, and
# check that the bad item is recorded while the rest still reduce.
#
# The deliberately-bad item is the point. A campaign that only works when
# every chunk is clean is not the feature — "a bad chunk doesn't kill the
# whole run" is, so the smoke test exercises that path rather than only the
# happy one.
#
# Resume (interrupt mid-run, pick up without repeating concluded items) has
# a real, load-bearing test at the Rust level in
# crates/cuttlefish-host/tests/fanout.rs. This script is an end-to-end sanity
# check of the CLI/spec layer on top of that mechanism, not a second proof
# of resume itself.
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
  mkdir -p "$work/corpus"

  cf=./target/debug/cuttlefish
  cfd=./target/debug/cuttlefishd

  # --- blocks ---------------------------------------------------------
  # map doubles n, and throws on the sentinel value so one item concludes
  # in failure without taking the run with it.
  mkdir -p "$work/blocks"
  cat > "$work/blocks/double.rhai" <<RHAI
//! signature: json -> json
if input.n == 0 { throw "bad chunk: n must be non-zero"; }
#{ doubled: input.n * 2 }
RHAI

  # reduce consumes the collection record and counts the results it can
  # actually read. Note: no trim() -- Rhai trim mutates in place and
  # returns unit, so `line.trim() != ""` is always true.
  cat > "$work/blocks/tally.rhai" <<RHAI
//! signature: {results_path: text, failures_path: text, succeeded: json, failed: json} -> json
let f = open(input.results_path);
let s = slice(f.handle, 0, f.len);
let n = 0;
for line in s.text.split("\n") {
    if line != "" { n += 1; }
}
#{ counted: n, succeeded: input.succeeded, failed: input.failed }
RHAI

  "$cf" catalog add double@1 "$work/blocks/double.rhai"
  "$cf" catalog add tally@1 "$work/blocks/tally.rhai"

  # --- manifest: three good items, one bad ----------------------------
  cat > "$work/corpus/manifest.jsonl" <<JSONL
{"n": 1}
{"n": 2}
{"n": 0}
{"n": 4}
JSONL

  cat > "$work/campaign.cuttlefish" <<SPEC
spec tally_campaign = {
  description = "Use when a manifest of items needs the same block run over each and the results tallied.";
  model = Stub "unused";
  data_policy = Local_only;
  capabilities = [ Read "./corpus" ];
  nodes = {
    analyze = { block = "double@1"; over = "./corpus/manifest.jsonl"; };
    tally   = { block = "tally@1"; in = analyze.out; };
  };
}
SPEC

  hash=$(pwd -P | shasum -a 256 | cut -c1-16)
  endpoint="${TMPDIR:-/tmp}/cuttlefish-campaign-$hash.sock"
  rm -f "$endpoint"

  "$cfd" "$work/campaign.cuttlefish" "$endpoint" > "$work/daemon.log" 2>&1 &
  daemon_pid=$!

  for _ in $(seq 1 100); do
    "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && break
    sleep 0.1
  done
  "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 || {
    echo "daemon never came up; log:"; cat "$work/daemon.log"; exit 1; }

  echo "--- running the campaign (4 items, one deliberately bad) ---"
  if ! out=$("$cf" run --endpoint "$endpoint" --spec tally_campaign --input "{}" 2>&1); then
    echo "$out"; echo "--- daemon log ---"; cat "$work/daemon.log"; exit 1
  fi
  echo "$out"

  counted=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"counted\"])")
  succeeded=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"succeeded\"])")
  failed=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"failed\"])")

  [ "$counted" = "3" ]   || { echo "FAIL: reduce counted $counted, expected 3"; exit 1; }
  [ "$succeeded" = "3" ] || { echo "FAIL: succeeded=$succeeded, expected 3"; exit 1; }
  [ "$failed" = "1" ]    || { echo "FAIL: failed=$failed, expected 1"; exit 1; }

  # The failure must be recorded, not merely tolerated.
  failures=$(find "$CUTTLEFISH_JOBS_HOME" -name "analyze.failures.jsonl" | head -1)
  grep -q "bad chunk" "$failures" || { echo "FAIL: failure not recorded in $failures"; exit 1; }
  echo "--- recorded failure ---"
  cat "$failures"

  "$cf" shutdown --endpoint "$endpoint" >/dev/null
  echo
  echo "OK: 3 of 4 items reduced, 1 bad chunk recorded without killing the run"
'
