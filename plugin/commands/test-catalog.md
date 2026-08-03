---
description: "Independently verify the cuttlefish block catalog CLI, then try to break it"
---

You're testing the block catalog (`cuttlefish catalog add/list/show/rm`) in
this repo. Use the `cuttlefish-catalog` skill for exact CLI syntax and
expected output shapes — don't read the Rust source to figure out how to
drive it.

Your job: independently verify it works as documented, then try to break it.
Don't just re-run the happy path — that's already covered by the test
suite. Specifically:

1. **Build**: `nix develop --command cargo build -p cuttlefish` and
   `nix develop --command cargo build -p cf-block-echo-summarize --target
   wasm32-unknown-unknown` (the real example block lives at
   `blocks/echo-summarize`).

2. **Happy path**: catalog the example block, list it, show it, remove it.
   Use a throwaway `$CUTTLEFISH_HOME` (a tempdir) — never touch the real
   `~/.cuttlefish`.

3. **Try to break it.** At minimum:
   - Catalog the same `name@version` twice — confirm it's rejected, not
     silently overwritten.
   - Look up something close to (but not exactly) something you cataloged —
     confirm the error suggests it.
   - Catalog a file that's neither valid wasm nor a `.cfbundle` — confirm a
     clear rejection, not silent acceptance or a crash.
   - Hand-edit `index.json` in the catalog directory to make it invalid
     JSON — confirm the next command reports the catalog as corrupt, not
     silently empty.
   - Run two `catalog add` calls at the same time (background one with `&`)
     targeting the same catalog directory with two *different* names —
     confirm both land.
   - Catalog a block with no declared signature — check for the warning
     line, not just silent success.
   - `catalog rm` something already removed, or never cataloged — confirm a
     clear error, not a silent no-op.

4. **Report**: for each thing tried, say what you expected, what actually
   happened, and whether they matched. Call out anything surprising
   explicitly — a confusing error message, a case that should have failed
   and didn't (or vice versa). If everything really did work, say
   specifically what you tried that could plausibly have found a problem
   and didn't — don't just say "everything worked."

Black-box only — don't modify code. If you find something that looks like a
real bug, report it clearly (command run, expected vs. actual, exit code)
rather than fixing it.
