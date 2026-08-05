---
description: "Independently verify cuttlefish block new (both authoring paths), then try to break it"
---

You're testing the cuttlefish-author flow (scaffolding a new proc block as
a Rhai script or a Rust crate, cataloging it, running real jobs against it)
in this repo. Use the `cuttlefish-author` skill for exact CLI syntax and
the Rhai language basics — don't read the Rust source to figure out how to
drive it.

Your job: independently verify it works as documented, then try to break
it. Don't just re-run `./try-author.sh` — that's already covered.
Specifically:

1. **Build**: `nix develop --command cargo build -p cuttlefish -p
   cuttlefishd` (per the skill's Build section).

2. **Happy path, both languages**: from a clean project directory (no
   `.cuttlefish/` yet), scaffold a block with `--lang rhai` (the default),
   catalog it, start a daemon against a spec referencing it, submit a job,
   confirm the identity result. Then edit the script to call `infer(...)`,
   catalog under a new version, confirm a real model round-trip works.
   Separately, scaffold a block with `--lang rust`, build it, catalog it,
   and confirm it runs too.

3. **Try to break it.** At minimum:
   - Scaffold with a name that already exists — expect a clear rejection,
     not an overwrite.
   - Scaffold with an invalid name: one that starts with a digit, one
     containing `.`, and a Windows-reserved device name (`con`, `aux`,
     `nul`, etc., case-insensitively) — expect a clear rejection for each,
     not a silent success followed by a confusing failure later.
   - Scaffold with an unparseable `--input`/`--output` type string — expect
     a clear parse error naming what's wrong, not a panic or a stack trace.
   - Catalog a `.rhai` file with no `//! signature: ...` header, and
     separately one with a header that doesn't parse — expect a clear
     rejection at `catalog add` time, not later when a spec references it.
   - Catalog a `.rhai` file with *two* `//! signature: ...` header lines —
     expect a clear rejection naming the ambiguity, not a silent pick of
     whichever came first.
   - Write a script that wraps an `infer(...)` call in `try { } catch { }`
     — per the skill's determinism rules, this is documented as breaking
     the replay mechanism. Confirm what actually happens (does it fail
     loudly, hang, or silently misbehave?) and whether that matches what
     the skill says to expect.
   - Reference an uncataloged `.rhai` file directly from a spec node
     (skipping `catalog add`, the way a `.wasm` block *can* be referenced
     by path) — confirm this is genuinely rejected as unsupported, not
     silently working by accident.
   - Run `cuttlefish build` against a spec containing a Script-kind node —
     expect a clear rejection naming the node, not a bundle that silently
     embeds a redundant copy of the shared interpreter and drops the
     actual script.
   - Run two projects' worth of this flow in two different temp
     directories at once — confirm no interference (different
     `.cuttlefish/blocks/`, independent catalogs if `CUTTLEFISH_HOME` is
     also scoped per project).

4. **Report**: for each thing tried, say what you expected, what actually
   happened, and whether they matched. Call out anything surprising
   explicitly — a confusing error message, a case that should have failed
   and didn't (or vice versa). If everything really did work, say
   specifically what you tried that could plausibly have found a problem
   and didn't — don't just say "everything worked."

Black-box only — don't modify code. If you find something that looks like
a real bug, report it clearly (command run, expected vs. actual, exit
code) rather than fixing it.
