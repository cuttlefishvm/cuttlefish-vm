---
description: "Independently verify the cuttlefish-run daemon lifecycle, then try to break it"
---

You're testing the cuttlefish-run flow (starting cuttlefishd, submitting
jobs without blocking, auto-resume) in this repo. Use the `cuttlefish-run`
skill for exact CLI syntax and the daemon-lifecycle procedure — don't read
the Rust source to figure out how to drive it.

Your job: independently verify it works as documented, then try to break
it. Don't just re-run `./try-run.sh` — that's already covered. Specifically:

1. **Build**: `nix develop --command cargo build -p cuttlefish -p
   cuttlefishd` (per the skill's Build section) and `nix develop --command
   cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown`
   (the example block you'll submit jobs against, at
   `blocks/echo-summarize`).

2. **Happy path**: follow the skill's "Ensuring a daemon is running"
   procedure from a clean project directory (no `.cuttlefish/` yet),
   submit a job, confirm it completes.

3. **Try to break it.** At minimum:
   - Run the ensure-daemon procedure a second time immediately after the
     first, with no daemon activity in between — confirm it reuses the
     existing daemon (check the PID in `daemon.json` is unchanged) rather
     than starting a redundant second one.
   - Corrupt `daemon.json` (garbage JSON, or a PID that belongs to some
     other real process on the machine) and run the procedure again —
     confirm it's treated as "no daemon" and a fresh one starts, rather
     than erroring out or, worse, treating an unrelated process as the
     daemon.
   - Ask for a job against a different spec than whatever daemon is
     currently running — confirm the old daemon is stopped (its PID is no
     longer live) and a new one starts for the new spec.
   - Kill the daemon process directly (not via `cuttlefish shutdown`)
     while a job is genuinely still running, then run the ensure-daemon
     procedure again for the same spec — confirm the job shows up as
     `Interrupted` via `cuttlefish jobs` immediately after restart, and is
     auto-resumed without you calling `resume` yourself.
   - Call `cuttlefish resume` on a job that's already `Completed` —
     confirm a clear rejection, not silent re-execution.
   - Run two projects' worth of this flow in two different temp
     directories at once — confirm they don't interfere (different
     `daemon.json`s, different job lists, no port/socket collision).

4. **Report**: for each thing tried, say what you expected, what actually
   happened, and whether they matched. Call out anything surprising
   explicitly. If everything really did work, say specifically what you
   tried that could plausibly have found a problem and didn't.

Black-box only — don't modify code. If you find something that looks like
a real bug, report it clearly (command run, expected vs. actual, exit
code) rather than fixing it.
