#!/usr/bin/env bash
# Exercise acceptance and recovery end to end: a fan-out node whose output
# must satisfy a JSON Schema beyond its declared type, with an on_fail ladder
# that gives up on the one item no amount of retrying will fix.
#
# The escalation is the point. Anything can survive work that succeeds; what
# this checks is that when cuttlefish gives up, it says so somewhere that
# outlives the session — `cuttlefish escalations` reports it afterwards, with
# the failing check's own text, from a plain job id nobody had to remember.
#
# What this does NOT prove: the retry *count*, or that nothing is
# checkpointed mid-climb. Neither is observable from the CLI — a retried
# attempt leaves no trace by design. Both have real tests at the Rust level
# in crates/cuttlefish-host/tests/ladder.rs. This is an end-to-end sanity
# check of the spec/CLI layer on top of that mechanism.
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

  # --- block ----------------------------------------------------------
  # Deterministically echoes its input as a score. Nothing here is wrong
  # per its declared type -- `json -> json` accepts any of it. The schema
  # below is what decides whether a given score is good enough, which is
  # exactly the gap `accept` exists to close.
  cat > "$work/blocks/grade.rhai" <<RHAI
//! signature: json -> json
#{ score: input.n }
RHAI
  "$cf" catalog add grade@1 "$work/blocks/grade.rhai"

  cat > "$work/schemas/score.json" <<JSON
{"type":"object","required":["score"],"properties":{"score":{"type":"integer","minimum":5}}}
JSON

  # --- manifest: two acceptable items, one that never will be ----------
  cat > "$work/corpus/manifest.jsonl" <<JSONL
{"n": 7}
{"n": 2}
{"n": 9}
JSONL

  cat > "$work/recovery.cuttlefish" <<SPEC
spec graded_campaign = {
  description = "Use when each item of a manifest must be graded and only scores of 5 or better may be kept.";
  model = Stub "unused";
  data_policy = Local_only;
  capabilities = [ Read "./corpus", Read "./schemas" ];
  nodes = {
    analyze = {
      block   = "grade@1";
      over    = "./corpus/manifest.jsonl";
      accept  = [ Schema "./schemas/score.json" ];
      on_fail = [ retry 1, escalate ];
    };
  };
}
SPEC

  hash=$(pwd -P | shasum -a 256 | cut -c1-16)
  endpoint="${TMPDIR:-/tmp}/cuttlefish-recovery-$hash.sock"
  rm -f "$endpoint"

  "$cfd" "$work/recovery.cuttlefish" "$endpoint" > "$work/daemon.log" 2>&1 &
  daemon_pid=$!

  for _ in $(seq 1 100); do
    "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && break
    sleep 0.1
  done
  "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 || {
    echo "daemon never came up; log:"; cat "$work/daemon.log"; exit 1; }

  echo "--- running (3 items, one that no retry can rescue) ---"
  if ! out=$("$cf" run --endpoint "$endpoint" --spec graded_campaign --input "{}" 2>&1); then
    echo "$out"; echo "--- daemon log ---"; cat "$work/daemon.log"; exit 1
  fi
  echo "$out"

  succeeded=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"succeeded\"])")
  failed=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"result\"][\"failed\"])")

  [ "$succeeded" = "2" ] || { echo "FAIL: succeeded=$succeeded, expected 2"; exit 1; }
  [ "$failed" = "1" ]    || { echo "FAIL: failed=$failed, expected 1"; exit 1; }

  # The give-up must be reported afterwards, by a command that was told
  # nothing about which job to look in.
  echo "--- cuttlefish escalations ---"
  esc=$("$cf" escalations --endpoint "$endpoint")
  echo "$esc"

  echo "$esc" | grep -q "analyze\[1\]" || {
    echo "FAIL: escalations did not name the failing item"; exit 1; }
  # The reason has to carry the failing check, or it is unactionable.
  echo "$esc" | grep -q "score" || {
    echo "FAIL: escalation reason does not name the failing field"; exit 1; }

  # An escalated item is still a concluded failure, so it is in
  # failures.jsonl like any other -- the escalation is an index into
  # those, not a separate outcome that replaces them.
  failures=$(find "$CUTTLEFISH_JOBS_HOME" -name "analyze.failures.jsonl" | head -1)
  grep -q "score" "$failures" || {
    echo "FAIL: escalated item missing from $failures"; exit 1; }
  echo "--- recorded failure ---"
  cat "$failures"

  "$cf" shutdown --endpoint "$endpoint" >/dev/null
  echo
  echo "OK: 2 of 3 items accepted, 1 escalated after its ladder ran out and reported afterwards"
'
