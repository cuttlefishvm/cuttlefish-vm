---
name: cuttlefish-test-run
description: Use when independently testing or adversarially verifying Cuttlefish daemon startup, reuse, metadata recovery, spec switching, interrupted-job resume, or project isolation
---

# Test the Cuttlefish daemon lifecycle

Test the `cuttlefish-run` flow as a black box. Use that skill for exact
daemon, endpoint, job, resume, and shutdown commands. Do not read the Rust
source to determine how to drive the flow.

## Safety and setup

Create one validated temporary root and set `CUTTLEFISH_HOME` beneath it
before starting Cuttlefish. Put each project's endpoint, metadata, log, and
`CUTTLEFISH_JOBS_HOME` in project-scoped test locations; never use the
platform default endpoint. On Unix, derive each endpoint from a short hash
of the canonical project path beneath `${TMPDIR:-/tmp}` to avoid socket
length limits; use the named-pipe equivalent on Windows. Track these
external socket paths explicitly. Bound every startup, shutdown, and job
poll.

Capture child PIDs and start identities directly when launching test
daemons and the unrelated sentinel. Install an EXIT trap that first uses
endpoint shutdown for test-owned daemons, then after a bounded wait uses
TERM and finally KILL only for the same verified captured child identity.
Terminate the sentinel only by its captured identity, remove tracked test
sockets, and delete only the validated temporary root. Never signal a PID
merely because it appears in `daemon.json`.

Build through the pinned toolchain:

```console
nix develop --command cargo build -p cuttlefish -p cuttlefishd
nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
```

Resolve the repository root and invoke its absolute freshly built
`cuttlefish` and `cuttlefishd` binaries. Use specs with distinct `Stub`
replies so jobs are deterministic and require no external model service.

## Acceptance matrix

1. From a clean project, follow the documented ensure-daemon procedure.
   Require a live captured child, responsive project endpoint, correct
   spec, project-local ledger, and a job with the exact Stub result.

2. Ensure the same spec again. Require the same PID, endpoint, and start
   identity, no second daemon, and another successfully completed job.

3. Test malformed JSON, wrong-shaped metadata, a dead owned PID, and a
   live unrelated sentinel PID paired with a dead endpoint. Each must be
   treated as stale and replaced without signaling the recorded unrelated
   PID.

4. Cross-wire a live unrelated PID with another test daemon's live
   endpoint. The flow must not assume independent liveness proves common
   ownership, reuse the other project, or shut either unrelated process
   down. Report inability to bind endpoint ownership to process identity
   as a safety failure.

5. Request a different spec in the same project. Require graceful shutdown
   through the old endpoint, bounded proof its captured child exited, a new
   PID serving only the new spec, updated metadata, and the new exact Stub
   result.

6. Generate a JSONL manifest of `{path: text}` inputs under the temporary
   root and a Stub-backed spec whose echo-summarize node fans out with
   `over = "<manifest>"`. Use absolute input paths and declare one `Read`
   capability covering both the manifest and every referenced file.
   Increase the manifest size until the submitted job is observably
   `Running`. Kill only the captured test daemon after
   verifying its identity. Restart the same spec and ledger, require the
   original job ID to become `Interrupted`, then follow the automatic
   resume procedure. It must reach `Completed` under the same ID. Across
   the node's results and failures, concluded item indexes must be unique
   and cover every manifest line exactly once. If the job finishes before
   the kill, increase the workload and retry; do not claim resume coverage
   without observing `Running` then `Interrupted`.

7. Resume a completed job. Require a clear nonzero rejection, no silent
   re-execution, and unchanged completed status and result.

8. Run two projects simultaneously with distinct endpoints, children,
   metadata, logs, ledgers, and Stub replies. Each daemon must expose only
   its own jobs. Shutting down and restarting one must leave the other's
   PID, endpoint, and history unchanged.

9. Prove the unrelated sentinel survived every stale and cross-wire probe,
   then terminate only that captured child. Stop every reachable test
   daemon with the bounded cleanup sequence, prove all captured children
   and tracked endpoints are gone, preserve logs long enough to report any
   failure, remove the temporary root, and verify cleanup completed.

## Report contract

For every probe, report the command or action, expected state, actual exit
code, PID, endpoint and job status, and whether they matched. Call out
confusing diagnostics, unexpected state transitions, and any situation
where process ownership cannot be proven.

Do not modify product code. Report bugs with a minimal reproduction rather
than fixing them.
