# Test prompt: exercise the new block catalog

Copy the block below to an agent (a fresh Claude Code session, or any coding
agent) to get an independent read on how far the block-catalog feature is.
Point it at this repo, on branch `feat/block-catalog` (or wherever it's
landed by the time you run this).

---

## Prompt

You're testing a newly-built feature in this repo: a local block catalog
(`cuttlefish catalog add/list/show/rm`), reachable via the `cuttlefish` CLI
binary. Read `harness/catalog-cli-reference.md` first — it has the exact CLI
syntax and expected output shapes, so you don't need to read the Rust source
to know how to drive it.

Your job: independently verify this works as documented, then try to break
it. Don't just re-run the happy path from the reference doc — that's already
been checked. Specifically:

1. **Build it**: `nix develop --command cargo build -p cuttlefish` and
   `nix develop --command cargo build -p cf-block-echo-summarize --target
   wasm32-unknown-unknown` (there's a real example block already in this
   repo at `blocks/echo-summarize` you can catalog).

2. **Happy path**: catalog the example block, list it, show it, remove it.
   Use a throwaway `$CUTTLEFISH_HOME` (a tempdir) so you don't touch your
   real `~/.cuttlefish`.

3. **Try to break it.** At minimum:
   - Catalog the same `name@version` twice — confirm it's rejected, not
     silently overwritten.
   - Look up something that doesn't exist, with a name close to (but not
     exactly) something you did catalog — confirm the error suggests it.
   - Catalog a file that's neither valid wasm nor a `.cfbundle` (e.g. a text
     file) — confirm it's rejected with a clear reason, not silently
     accepted or a crash.
   - Hand-edit `index.json` in the catalog directory to make it invalid JSON
     — confirm the next `catalog` command reports the catalog as corrupt,
     not as silently empty.
   - Try two `catalog add` calls at the same time (e.g. `&` in bash to
     background one) targeting the same catalog directory with two
     *different* names — confirm both land.
   - Catalog a block that has no declared signature — check for the warning
     line, not just a silent success.
   - Try `catalog rm` on something already removed, or never cataloged —
     confirm it's a clear error, not a silent no-op.

4. **Report back**: for each thing you tried, say what you expected, what
   actually happened, and whether they matched. If anything surprised you —
   an error message that was confusing, a case that should have failed and
   didn't (or vice versa), behavior not covered by the reference doc — call
   it out explicitly. Don't just say "everything worked"; if everything
   really did work, say specifically what you tried that could plausibly
   have found a problem and didn't.

You do not need to modify any code — this is a black-box exercise of the
CLI as a user would experience it. If you find something that looks like a
real bug, don't fix it — just report it clearly (command run, expected vs.
actual, exit code) so it can be triaged.
