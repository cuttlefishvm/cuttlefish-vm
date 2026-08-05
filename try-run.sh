#!/usr/bin/env bash
# Exercise the cuttlefish-run flow end to end: start a daemon, submit a
# job without waiting, watch it complete via `jobs`, kill the daemon
# (after the job has already finished — this script does NOT catch a job
# genuinely mid-flight; see the note below), restart it, and shut it down
# cleanly. Does not exercise a spec-switch restart.
#
# The genuinely-caught-mid-job auto-resume case already has a real,
# load-bearing test at the Rust level (crates/cuttlefish-host/tests/runner.rs,
# from the DAG-core work this feature builds on) — this script is an
# end-to-end sanity check of the CLI/skill layer on top of that mechanism,
# not a second proof that resume itself works.
set -euo pipefail
cd "$(dirname "$0")"

nix develop --command bash -c '
  set -eo pipefail
  cargo build -q -p cf-block-echo-summarize --target wasm32-unknown-unknown
  cargo build -q -p cuttlefish -p cuttlefishd

  # Everything this run creates — the daemon jobs ledger, the throwaway
  # spec, the endpoint socket, and both daemon logs — lives under one work
  # dir so a single trap cleans it all up, including on a failure partway
  # through. The trap also has to kill any daemon(s) still running at exit
  # time — deleting $work out from under a live cuttlefishd only orphans
  # it holding the socket open, it does not stop it — so both PIDs are
  # tracked from before either daemon starts (empty is fine: `kill -9`
  # on an unset/empty PID is a no-op courtesy of the unquoted expansion
  # below dropping empty words, and is silenced either way).
  daemon_pid=""
  daemon_pid2=""
  work="$(mktemp -d)"
  trap '\''
    _trap_exit_code=$?
    if [ "$_trap_exit_code" -ne 0 ]; then
      for log in "$work/daemon.log" "$work/daemon2.log"; do
        if [ -s "$log" ]; then
          echo
          echo "--- $log (tail, on failure) ---"
          tail -n 50 "$log"
        fi
      done
    fi
    kill -9 $daemon_pid $daemon_pid2 2>/dev/null || true
    rm -rf "$work"
    exit "$_trap_exit_code"
  '\'' EXIT
  export CUTTLEFISH_JOBS_HOME="$work/jobs"
  cf=./target/debug/cuttlefish
  cfd=./target/debug/cuttlefishd
  # An explicit, project-scoped endpoint — never the platform default — per
  # plugin/skills/cuttlefish-run/SKILL.md'\''s guidance: the default endpoint is
  # a single well-known path, so two daemons started against it (e.g. this
  # script running alongside anything else on the machine) would collide.
  endpoint="$work/daemon.sock"
  spec="$work/spec.cuttlefish"
  # Absolute, not relative: a spec resolves a relative `block` path against
  # the spec file'\''s own directory (spec_dir.join(...) in cuttlefishd
  # main.rs), which is $work here, not the repo root.
  wasm="$(pwd)/target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm"

  cat > "$spec" <<EOF
spec try_run = {
  description = "try-run.sh smoke test";
  model = Stub "ok";
  data_policy = Local_only;
  capabilities = [ Read "." ];
  block = "$wasm";
}
EOF

  echo
  echo "--- start a daemon ---"
  "$cfd" "$spec" "$endpoint" > "$work/daemon.log" 2>&1 &
  daemon_pid=$!
  for _ in $(seq 1 50); do
    "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && break
    sleep 0.1
  done
  "$cf" specs --endpoint "$endpoint" >/dev/null
  echo "daemon up: ok"

  echo
  echo "--- submit without waiting, then check on it ---"
  job_id="$("$cf" submit --endpoint "$endpoint" --spec try_run --input "{\"path\": \"$spec\"}")"
  echo "submitted: $job_id"
  status="missing"
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
  echo "final status: $status"
  [ "$status" = "completed" ]
  echo "job completed: ok"

  echo
  echo "--- kill the daemon, restart, confirm it comes back up ---"
  kill -9 "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
  "$cfd" "$spec" "$endpoint" > "$work/daemon2.log" 2>&1 &
  daemon_pid2=$!
  for _ in $(seq 1 50); do
    "$cf" specs --endpoint "$endpoint" >/dev/null 2>&1 && break
    sleep 0.1
  done
  "$cf" specs --endpoint "$endpoint" >/dev/null
  echo "restarted: ok"

  echo
  echo "--- shut it down cleanly ---"
  "$cf" shutdown --endpoint "$endpoint"
  wait "$daemon_pid2" 2>/dev/null || true
  echo "shutdown: ok"
'
