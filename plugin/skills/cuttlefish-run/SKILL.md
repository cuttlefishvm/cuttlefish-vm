---
name: cuttlefish-run
description: Use when starting cuttlefishd, submitting a job without waiting for it, checking on running/interrupted jobs, or resuming/cancelling one — anything that drives a live daemon rather than local-only catalog/build operations
---

# cuttlefish-run

## Overview

Drives a live `cuttlefishd` end to end: starts one if none is running for
the spec you need, submits jobs without blocking, and picks crashed work
back up automatically. Project-scoped — every project directory gets its
own daemon, endpoint, and job history under `<project>/.cuttlefish/`, so
two different repos never collide and two Claude Code sessions in the same
repo share one daemon and see the same jobs. That isolation depends on
always starting the daemon on an explicit, project-scoped endpoint — see
"Starting" below; the platform default endpoint is a single fixed
path/pipe name shared by every project on the machine and is never safe
to use here.

## Build

**REQUIRED SUB-SKILL:** Use cuttlefish-cli to get the `cuttlefish`/
`cuttlefishd` binaries before proceeding.

## State: `<project_root>/.cuttlefish/`

Gitignored (add a `.gitignore` entry here if the project doesn't already
have one — check first). Holds:

- `daemon.json` — `{pid, spec_path, endpoint, started_at}` for whichever
  daemon this project is currently running, if any.
- `jobs/` — every job's on-disk ledger, via `CUTTLEFISH_JOBS_HOME` (set
  when the daemon is started — see below). The block *catalog* stays at
  the global `~/.cuttlefish/catalog` (or `$CUTTLEFISH_HOME/catalog`) —
  deliberately not project-scoped, since a cataloged block is reusable
  across every project.
- `daemon.log` — the running daemon's stderr/stdout, for debugging a
  startup failure.

## Ensuring a daemon is running for a spec

Before submitting a job against `<spec>`, run this procedure:

1. **Read `daemon.json`** if it exists.
2. **Probe it, don't trust it.** Check the recorded `pid` is a live
   process (e.g. `kill -0 <pid>` on unix; an equivalent process check on
   Windows) *and* `cuttlefish specs --endpoint <recorded endpoint>`
   succeeds. If either check fails, treat this as "no daemon" and delete
   the stale `daemon.json` rather than acting on it — a dead PID reused
   by an unrelated process is a real, if rare, risk worth guarding
   against.
3. **If a live daemon is confirmed and its `spec_path` matches `<spec>`:**
   reuse it — skip to "Auto-resume" below, then proceed.
4. **If a live daemon is confirmed but bound to a different spec:** run
   `cuttlefish shutdown --endpoint <recorded endpoint>`, wait briefly
   (poll `cuttlefish specs` against that endpoint until it fails, bounded
   — a few seconds is plenty), then start fresh (step 5). Any of the old
   spec's still-running jobs are cut off, not lost — they'll show up as
   `Interrupted` the next time a daemon starts for *that* spec, and the
   daemon's own graph-fingerprint check refuses to resume them against
   the wrong spec in the meantime.
5. **If no live daemon:** start one.

**Starting:**

Always pass an explicit, project-scoped `[endpoint]` — never omit it.
`cuttlefishd`'s platform default endpoint
(`cuttlefish_core::endpoint::default_endpoint()`) is a single fixed path
(`/tmp/cuttlefish.sock` on unix) or pipe name shared by *every* project on
the machine, not scoped per project. Worse, on unix `cuttlefishd` does an
unconditional `std::fs::remove_file` on its endpoint before binding, with
no check for a live process still listening there — so a second project
started at the default endpoint silently unlinks and steals the first
project's socket, orphaning the first daemon as an unreachable zombie
instead of failing loudly. This is exactly the collision the Overview
promises never happens, and it only doesn't happen if every daemon gets
its own endpoint.

On unix, scope the endpoint to the project directory itself:

```bash
ENDPOINT="<project_root>/.cuttlefish/daemon.sock"
CUTTLEFISH_JOBS_HOME="<project_root>/.cuttlefish/jobs" \
  ./target/debug/cuttlefishd <spec> "$ENDPOINT" \
  > <project_root>/.cuttlefish/daemon.log 2>&1 &
echo $! # the PID to record
```

On Windows a named pipe has no filesystem nesting — it's a flat name in
the pipe namespace, not a path under `<project_root>`. Derive a
project-unique name instead, e.g. a short hash of the canonicalized
project root: `\\.\pipe\cuttlefish-<hash-of-project-root>`.
(`cuttlefishd`'s named-pipe listener binds with `first_pipe_instance`, so
on Windows an actual name collision fails loudly at startup rather than
silently taking over the way unix's `remove_file` does — but a
project-unique name avoids ever hitting that failure in the first place,
and keeps the same one-daemon-per-project guarantee unix gets from a
project-scoped socket path.)

(`CUTTLEFISH_HOME` is left unset/inherited — the catalog stays global.
`--wasm <path>` is never passed here — it's only for overriding a node's
compiled bytes, which this flow never needs.)

Write `daemon.json` with the new `{pid, spec_path, endpoint, started_at}`.
Wait (briefly, bounded) for `cuttlefish specs --endpoint <endpoint>` to
succeed before considering it up — a spec parse error or a bundle-stage
rejection makes `cuttlefishd` exit immediately; check `daemon.log` if the
probe never succeeds. Once the daemon is confirmed up, proceed to
Auto-resume below.

## Auto-resume

**Every time** a daemon is confirmed live for the spec you need — whether
just freshly started or reused from step 3 above — run:

```bash
cuttlefish jobs --endpoint <endpoint>
```

For every job in the result with `"status": "interrupted"`:

```bash
cuttlefish resume --endpoint <endpoint> <job_id> || true
```

This is deliberately cheap to repeat on every invocation, not just once
per process launch: `jobs` is a cheap read, and the daemon refuses (`409
Conflict`) a `resume` against a job that isn't `Interrupted` anymore. That
refusal is **not silent at the CLI** — `cuttlefish resume` exits non-zero
and prints something like `Error: daemon rejected the resume: 409
Conflict {"error":"only an Interrupted job can be resumed"}` to stderr.
Because multiple sessions can share one daemon (see Overview), this is an
expected race, not a bug: two sessions can both see the same job sitting
at `Interrupted` and both call `resume` on it — the loser gets exactly
this 409. Treat *only* this specific failure as ignorable in the
auto-resume loop (the `|| true` above, or whatever's equivalent for the
loop actually driving this) — it means someone else already resumed the
job, not that anything went wrong. Do not broaden this into ignoring
`resume` errors in general: a resume that fails for some other reason
(daemon unreachable, unknown job_id, wrong spec) should still surface as
the fatal error it is.

`cuttlefishd` itself never does this unprompted — this auto-resume step
*is* the "someone decided to resume it" signal the daemon's design
requires, just automated at the skill layer instead of left to a human
each time.

## Submitting and checking on jobs

```bash
cuttlefish submit --endpoint <endpoint> --spec <name> --input '<json>'
# prints a job_id immediately — does not wait for completion

cuttlefish jobs --endpoint <endpoint>
# lists every job and its status — check on things whenever you want

cuttlefish cancel --endpoint <endpoint> <job_id>
```

`cuttlefish run` (unchanged, pre-existing) still blocks until one specific
job finishes — use it only when actually waiting for a single result is
the right thing to do, not as the default way to kick off work.

## Running as a dispatched agent

For an actual job run — not a quick one-off check like `cuttlefish jobs`
— prefer dispatching the `cuttlefish-runner` agent (via the `Agent` tool)
rather than following this skill's ensure-daemon/submit/poll/auto-resume
procedure inline. It's the same procedure, already baked into that
agent's own system prompt — dispatching it skips loading this skill at
all. Give it the project root, the resolved `BIN_DIR` (dispatch
`cuttlefish-binary-resolver` first if binaries aren't already known to be
on `PATH`), the spec name/path, and the input JSON; it reports back the
job's final result. Two reasons to prefer this over driving it inline:

- **Transcript identity.** Driving the full lifecycle inline renders as
  an anonymous run of collapsed "Ran N shell commands" entries,
  indistinguishable from anything else the session did. A dispatched
  subagent gets its own named, trackable block — the same rendering
  `Agent`-dispatched work already gets everywhere else.
- **Clean handoff.** The calling session's transcript reduces to
  "dispatched a cuttlefish run, here's what it returned" instead of the
  raw polling loop.

This is a recommendation, not a requirement — a quick status check, or
anything already running inside a test harness (`test-run.md` and the
other `test-*.md` commands adversarially test *this checkout's* behavior
directly and should keep doing so inline, not through a layer of dispatch
that would obscure a failure's origin), still runs inline as before.

## Things worth knowing

- A daemon serves exactly one spec at a time. Needing a second spec means
  stopping the first (step 4 above), not running two side by side — this
  flow supports at most one live daemon per project.
- `daemon.json`/`.cuttlefish/jobs` are per-project. A second Claude Code
  session working in the *same* project directory shares this daemon and
  sees the same jobs; a session in a *different* project never touches
  this one.
- The block catalog (`cuttlefish catalog ...`) is unaffected by any of
  this — it's global, and doesn't need a daemon at all.

## Verifying end to end

`./try-run.sh` from the repo root runs a daemon-start / submit / list /
complete / kill-after-completion / restart / shutdown cycle — run it to see that
surface working before relying on it. It does *not* prove the
genuinely-caught-mid-job auto-resume case (see the script's own header
comment for why) — that mechanism already has a real, load-bearing test
at the Rust level from the DAG-core work this feature builds on; this
script is an end-to-end sanity check of the CLI/skill layer on top of
that, not a second proof of the underlying resume mechanism.
