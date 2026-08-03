---
description: "Independently verify the cuttlefish build CLI, then try to break it"
---

You're testing the pipeline linker (`cuttlefish build`) in this repo. Use the
`cuttlefish-build` skill for exact CLI syntax and expected output shapes —
don't read the Rust source to figure out how to drive it. The `cuttlefish-
catalog` skill covers `catalog add/list/show/rm`, which you'll also need.

Your job: independently verify it works as documented, then try to break it.
Don't just re-run the happy path — that's already covered by the test suite.
Specifically:

1. **Build**: `nix develop --command cargo build -p cuttlefish` and
   `nix develop --command cargo build -p cf-block-echo-summarize --target
   wasm32-unknown-unknown` (the real example block lives at
   `blocks/echo-summarize`).

2. **Happy path**: build `examples/summarize.cuttlefish` to a `.cfbundle`
   with `-o`, and confirm the output exists and the `checking node ... ok`
   / `built: ...` lines look right. Use a throwaway `$CUTTLEFISH_HOME` (a
   tempdir) — never touch the real `~/.cuttlefish`.

3. **Try to break it.** At minimum:
   - Build a spec whose single pipeline stage is a bare catalog name
     (`name@version`) that you cataloged yourself first — confirm it
     resolves through the catalog rather than being treated as a path.
   - Build the same spec twice to two different output paths and `cmp` the
     bytes — confirm they're byte-identical (no timestamps, no absolute
     paths baked in).
   - Catalog a `.cfbundle` you already built, then reference it by
     `name@version` as the (only) stage of a second spec, and build that.
     Confirm the CLI links and packages it without error — building embeds
     the bundle as a node, it does not execute anything inside it.
   - Point a spec's `block` at a `name@version` you never cataloged —
     confirm the error is a clear not-found-with-suggestion (or a plain
     not-found if nothing close exists in the catalog), not a raw panic or
     stack trace.
   - Construct a genuine seam mismatch: two blocks where the second's
     declared input isn't satisfied by the first's declared output (you can
     compile a tiny throwaway block crate against `cuttlefish-sdk`
     declaring whatever `Signature` you want — see
     `crates/cuttlefish-host/tests/support.rs`'s `block_with` for the
     minimal shape of such a crate, or just write one by hand). Confirm the
     error names *both* blocks and *both* types, not a generic type error.
   - Try to build a spec pointed at a spec file that doesn't exist, and one
     that fails to parse (bad syntax) — confirm both give a clear error
     naming the path, not a panic.
   - Run `cuttlefish build some.cuttlefish -o some.cuttlefish` (or any case
     where the output path resolves to the same file as the spec being
     built) — confirm it refuses rather than clobbering the source spec.
   - Build a spec whose pipeline has more than one stage where every seam
     lines up — confirm the bundle's node count and the `built: ...`
     summary line reflect all of them, not just the first or last.

4. **Report**: for each thing tried, say what you expected, what actually
   happened, and whether they matched. Call out anything surprising
   explicitly — a confusing error message, a case that should have failed
   and didn't (or vice versa). If everything really did work, say
   specifically what you tried that could plausibly have found a problem
   and didn't — don't just say "everything worked."

Black-box only — don't modify code. If you find something that looks like a
real bug, report it clearly (command run, expected vs. actual, exit code)
rather than fixing it.
