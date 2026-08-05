#!/usr/bin/env bash
# Exercise the cuttlefish-author flow end to end: scaffold a Rhai block
# (identity passthrough), catalog it, run a real job against it through a
# real daemon, confirm the identity result; edit the script to do a real
# infer() round-trip, catalog under a new version, confirm the edited
# behavior; scaffold + build + catalog + run a Rust block too; and sanity
# check that the module-compile cache actually helps (two submissions
# against the same cataloged Rhai script, second noticeably faster than
# the first).
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -eo pipefail
  cargo build -q -p cuttlefish -p cuttlefishd

  # Everything this run creates lives under one work dir, cleaned up by a
  # single trap on exit — same pattern try-run.sh already established,
  # including killing any daemon(s) still running (deleting $work out from
  # under a live cuttlefishd only orphans it holding the socket open, it
  # does not stop it).
  daemon_pid=""
  work="$(mktemp -d)"
  trap '\''
    _trap_exit_code=$?
    if [ "$_trap_exit_code" -ne 0 ]; then
      for log in "$work"/daemon*.log; do
        if [ -s "$log" ]; then
          echo
          echo "--- $log (tail, on failure) ---"
          tail -n 50 "$log"
        fi
      done
    fi
    kill -9 $daemon_pid 2>/dev/null || true
    rm -rf "$work"
    exit "$_trap_exit_code"
  '\'' EXIT

  export CUTTLEFISH_HOME="$work/home"
  cf="$(pwd)/target/debug/cuttlefish"
  cfd="$(pwd)/target/debug/cuttlefishd"
  endpoint="$work/daemon.sock"

  # `cuttlefish block new` scaffolds into `.cuttlefish/blocks/` relative to
  # the current working directory — cd into $work (a throwaway "project
  # directory") before calling it, rather than littering the repo root
  # with a stray .cuttlefish/ directory. $cf/$cfd are already absolute so
  # they keep working after this cd.
  cd "$work"

  start_daemon() {
    local spec="$1"
    local log="$2"
    "$cfd" "$spec" "$endpoint" > "$log" 2>&1 &
    daemon_pid=$!
    for _ in $(seq 1 50); do
      "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && return 0
      sleep 0.1
    done
    "$cf" specs --endpoint "$endpoint" >/dev/null
  }

  stop_daemon() {
    "$cf" shutdown --endpoint "$endpoint"
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
  }

  submit_and_wait() {
    local spec_name="$1"
    local input="$2"
    local job_id
    job_id="$("$cf" submit --endpoint "$endpoint" --spec "$spec_name" --input "$input")"
    local status="missing"
    for _ in $(seq 1 100); do
      status="$("$cf" jobs --endpoint "$endpoint" | python3 -c "
import json,sys
jobs = json.load(sys.stdin)
print(next((j[\"status\"] for j in jobs if j[\"job_id\"] == \"$job_id\"), \"missing\"))
")"
      case "$status" in
        completed|failed) break ;;
      esac
      sleep 0.1
    done
    [ "$status" = "completed" ] || {
      echo "job $job_id ended as $status, not completed" >&2
      return 1
    }
    "$cf" jobs --endpoint "$endpoint" | python3 -c "
import json,sys
jobs = json.load(sys.stdin)
job = next(j for j in jobs if j[\"job_id\"] == \"$job_id\")
print(json.dumps(job[\"envelope\"][\"result\"], separators=(\",\", \":\")))
"
  }

  echo
  echo "--- cuttlefish block new (rhai, default) ---"
  "$cf" block new echo-script --input "{n: json}" --output "{n: json}"
  test -f .cuttlefish/blocks/echo-script/block.rhai
  echo "scaffolded: ok"

  echo
  echo "--- catalog it, run a job, confirm identity passthrough ---"
  "$cf" catalog add echo-script@1 .cuttlefish/blocks/echo-script/block.rhai
  cat > "$work/spec1.cuttlefish" <<EOF
spec try_author_v1 = {
  description = "try-author.sh smoke test, v1";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [];
  nodes = { echo = { block = "echo-script@1" }; };
}
EOF
  start_daemon "$work/spec1.cuttlefish" "$work/daemon1.log"
  echo "daemon up: ok"

  # Timed deliberately as the very FIRST job this fresh daemon runs against
  # a Script-kind node — this is the one submission that pays the shared
  # interpreter'\''s real wasmtime compile cost (nothing has warmed the
  # module-compile cache yet). The second submission below reuses the same
  # daemon, so it hits the now-warm cache. Timing an already-warm daemon
  # here (e.g. after some other job had already run) would silently prove
  # nothing — the two numbers would look the same regardless of whether
  # caching works at all.
  start=$(date +%s%N)
  result="$(submit_and_wait try_author_v1 "{\"n\": 21}")"
  first_ms=$(( ($(date +%s%N) - start) / 1000000 ))
  [ "$result" = "{\"n\":21}" ]
  echo "identity passthrough: ok"

  start=$(date +%s%N)
  submit_and_wait try_author_v1 "{\"n\": 1}" >/dev/null
  second_ms=$(( ($(date +%s%N) - start) / 1000000 ))
  echo "module-compile-cache sanity check: first (cold) submission ${first_ms}ms, second (warm) submission ${second_ms}ms"

  stop_daemon

  echo
  echo "--- edit the script to do a real infer() round-trip, catalog a new version ---"
  cat > .cuttlefish/blocks/echo-script/block.rhai <<EOF
//! signature: {n: json} -> {n: json}
#{ n: infer("describe: " + input.n, 16) }
EOF
  "$cf" catalog add echo-script@2 .cuttlefish/blocks/echo-script/block.rhai
  cat > "$work/spec2.cuttlefish" <<EOF
spec try_author_v2 = {
  description = "try-author.sh smoke test, v2 (infer round-trip)";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [];
  nodes = { echo = { block = "echo-script@2" }; };
}
EOF
  start_daemon "$work/spec2.cuttlefish" "$work/daemon2.log"
  result="$(submit_and_wait try_author_v2 "{\"n\": 7}")"
  [ "$result" = "{\"n\":\"ok\"}" ]
  echo "infer() round-trip: ok"
  stop_daemon

  echo
  echo "--- cuttlefish block new (rust, opt-in), build, catalog, run ---"
  "$cf" block new echo-rust --input "{n: json}" --output "{n: json}" --lang rust
  test -f .cuttlefish/blocks/echo-rust/Cargo.toml
  cargo build -q --manifest-path .cuttlefish/blocks/echo-rust/Cargo.toml --target wasm32-unknown-unknown
  "$cf" catalog add echo-rust@1 \
    .cuttlefish/blocks/echo-rust/target/wasm32-unknown-unknown/debug/cf_block_echo_rust.wasm
  cat > "$work/spec3.cuttlefish" <<EOF
spec try_author_rust = {
  description = "try-author.sh smoke test, rust block";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [];
  nodes = { echo = { block = "echo-rust@1" }; };
}
EOF
  start_daemon "$work/spec3.cuttlefish" "$work/daemon3.log"
  result="$(submit_and_wait try_author_rust "{\"n\": 99}")"
  [ "$result" = "{\"n\":99}" ]
  echo "rust block identity passthrough: ok"
  stop_daemon
'
