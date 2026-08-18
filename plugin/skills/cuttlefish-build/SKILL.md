---
name: cuttlefish-build
description: Use when building, linking, or packaging a .cuttlefish spec into a .cfbundle via the cuttlefish CLI, or when asked about pipeline seam-checking, catalog-name resolution in a spec's pipeline, or reproducible/byte-identical builds
---

# cuttlefish-build

> **No `cuttlefish` on PATH? Download it from GitHub Releases.** That is
> the answer, not a last resort: dispatch the `cuttlefish-binary-resolver`
> agent, or follow `cuttlefish-cli` inline and run its download script. It
> resolves the latest tag, verifies a checksum, and caches per tag, so
> re-running costs nothing.
>
> Two things not to do. Don't report "binaries unavailable" — they are
> downloadable on every supported platform. And don't reuse an older
> cached tag such as `~/.cache/cuttlefish/bin/v0.0.7`; releases carry
> fixes that change behaviour, and one API call settles which tag is
> current.

## Overview

`cuttlefish build` links and verifies a spec's pipeline into a distributable
`.cfbundle`, reachable via the `cuttlefish build` subcommand. Purely local
filesystem operations — no daemon, no network.

## Build

**REQUIRED SUB-SKILL:** Use cuttlefish-cli to get the `cuttlefish` binary
before proceeding.

## Spec file syntax

A `.cuttlefish` file is `spec NAME = { field = value; ... }`. Unknown fields
are a hard error (deny-by-default), so this is the whole field list — don't
grep `crates/cuttlefish-core/src/spec.rs` for it:

```
spec summarize_docs = {
  description = "Use when the agent needs a summary of a local file and
                  the content must not leave the machine.";
  model = Ollama "llama3.2:1b";
  data_policy = Local_only;
  capabilities = [ Read "./docs" ];
  block = "../blocks/echo-summarize";
}
```

- `description` — trigger conditions for a *calling agent*, not an
  explanation of internals.
- `model = Provider "target"` — provider is a bare, case-insensitive ident
  (`Ollama`, `LlamaCpp`, `Stub`); target's meaning is provider-specific (a
  model tag for Ollama, a `.gguf` path for LlamaCpp, the canned reply for
  Stub). Naming a provider this build wasn't compiled with is a resolution
  error listing what's actually available — see the README's "Inference
  providers" table for the full list and feature-flag requirements.
  Before picking an Ollama tag, run `cuttlefish models list` — it lists
  every locally pulled model with a best-effort `reasoning` flag
  (`true`/`false`/unknown), so a job using `infer()` with a modest
  `max_tokens` doesn't accidentally land on a reasoning model that burns
  its whole budget on `<think>` output and returns an empty reply. Don't
  hand-probe models one at a time to rediscover this.
- `data_policy` — `Local_only` or `Any`. Discovery metadata for the calling
  agent (pass paths vs. contents); not itself an enforcement mechanism.
- `capabilities = [ Read "path", Fetch "https://host/path", ... ]` — the
  enforcement boundary, checked at startup and again at runtime.
  - **`Read`** grants a filesystem root a block may read beneath.
  - **`Fetch`** grants a URL *prefix*, matched against the URL as written,
    so `Fetch "https://x.org/docs/"` covers that subtree and not
    `https://x.org/other`, a different host, or the same path over `http://`.
    A URL containing `..` is refused.

  A job with no `Fetch` grant cannot reach the network at all, which is the
  default. Fan-out manifests and `accept` schemas must sit inside a `Read`
  root: the host reads those itself, so leaving them ungranted would make
  the capability list an untruthful account of what the job touches.
- `block = "..."` — sugar for a one-node graph. A path relative to the
  spec's own directory (or one ending `.wasm`/`.cfbundle`) reads straight
  from disk; anything else is a catalog `name@version` lookup (see
  `cuttlefish-catalog`).
- `nodes = { name = { block = "..."; in = <expr>; }; ... };` — the real
  multi-block graph `block = "..."` desugars to. Omit `in` for a node with
  no inbound edge (it gets the job's raw input); otherwise `in` is one of:

  - **`other_node.out`** — that node's *whole* output, unchanged. This is
    what you want whenever a node's declared input already matches an
    upstream node's declared output shape, which is the common case for a
    plain A→B→C chain:

    ```
    nodes = {
      analyst = { block = "analyst@1"; };
      # analyst outputs {segment, finding, risk}; stress declares that
      # same shape as its input -- pass it through as-is, no braces:
      stress = { block = "stress@1"; in = analyst.out; };
    };
    ```

    **Don't** wrap a single upstream node field by field —
    `in = { segment = analyst.out; finding = analyst.out; risk =
    analyst.out; }` nests analyst's *entire* output under every one of
    those three fields instead of using its fields directly, and gets
    rejected as a seam mismatch (correctly — the resulting type really is
    wrong, not a typechecker bug). If you hit this, the error names the
    fix. There's no syntax to pull out just *one* field of an upstream
    node's output (`analyst.segment` is not valid) — either the whole
    shape matches (use the bare form above) or it doesn't, in which case
    change one of the two blocks' signatures so it does.
  - **`{ field = <expr>; ... }`** — build a record by combining *multiple
    different* nodes (real fan-in), each field an independent expression:

    ```
    in = { docs = extract.out; images = render.out; };
    ```
  - **`[ <expr>, ... ]`** — build a list the same way, order significant.

  Typechecked the same way as any other seam (see "Command" below) before
  anything runs.

  A node may also declare **`over = "manifest.jsonl"`**, which makes it run
  its block **once per line** of that manifest instead of once — the whole
  campaign being one job, so it survives the agent that submitted it:

  ```
  nodes = {
    analyze = { block = "analyst@1"; over = "./corpus/manifest.jsonl"; };
    synth   = { block = "synth@1"; in = analyze.out; };
  };
  ```

  - The manifest is **JSONL**: one JSON value per line, each the complete
    input for one run. Blank lines are skipped. It must sit inside a path
    granted by `capabilities` (the host reads it, and the host isn't
    sandboxed, so this keeps the capability list honest).
  - A fan-out node's `.out` is **not** one item's result — it is the fixed
    record `{results_path: text, failures_path: text, succeeded: json,
    failed: json}`. A downstream node must declare *that* as its input;
    declaring the per-item shape is the natural mistake and won't
    typecheck. Read the results with `open`/`slice` on `results_path`.
  - **Each line of `results.jsonl` is `{"item": N, "result": {...}}`** — the
    block's own output is nested under `result`, not spliced into the line.
    Reading `r.my_field` instead of `r.result.my_field` yields unit and
    costs a run to discover. The wrapper is deliberate: `item` is what makes
    a result traceable back to its manifest line and cross-referenceable
    with `failures.jsonl`, whose lines are `{"item": N, "error": "..."}`.
  - **One bad item doesn't kill the run.** An item whose input doesn't
    match the declared type, or whose block fails, is recorded in
    `failures.jsonl` and the rest continue. What *does* fail the whole job:
    a missing/unparseable/empty manifest (an authoring error, caught before
    any item runs), or every single item failing.
  - **Resume doesn't repeat concluded work.** Interrupt a 500-item run and
    resume it; items that already succeeded or already failed are skipped,
    items that were merely in flight run again. Editing the manifest
    between runs is refused rather than silently reindexed — item numbers
    only mean something relative to one manifest.
  - `over` and `repeat_until` cannot be combined; they describe different
    kinds of iteration.

  A node may also declare **`accept`** — what "done" means beyond its type —
  and **`on_fail`** — what to do when a run isn't accepted:

  ```
  nodes = {
    extract = {
      block  = "extractor@1";
      over   = "./corpus/manifest.jsonl";
      accept = [
        Schema "./schemas/finding.json",
        Judge { model = Ollama "qwen3:8b"; prompt = "Does this cite numbers from the input?"; }
      ];
      on_fail = [ retry 2, reroute Ollama "qwen3:14b", escalate ];
    };
  };
  ```

  - **`accept = [ ... ]`** runs in order and stops at the first failure.
    Put `Schema` before `Judge`: a schema is free and a judge costs a whole
    inference, so structurally broken output should never pay for one.
    - `Schema "path.json"` — JSON Schema. The path must sit inside a
      `capabilities` read root, same as a manifest.
    - `Judge "prompt"` uses the spec's own model; `Judge { model = P "t";
      prompt = "..."; }` names another. The judge sees the node's **input
      and output**, and must answer `{"accept": bool, "reason": "..."}`.
      A reply that isn't a usable verdict is *not* a rejection — it is a
      broken grader, retried and reported as such.
  - **`on_fail = [ ... ]`** is an ordered ladder, climbed one rung per
    rejection: `retry N` (N further attempts on the same model),
    `reroute P "model"` (switch models for every attempt after this),
    `escalate` (stop, and record it for `cuttlefish escalations`).
    `escalate` must be last, and `retry 0` is refused.
  - **A node with no `on_fail` runs once and fails**, exactly as before.
  - **Nothing is checkpointed while the ladder is climbing.** Only a
    concluded outcome gets a ledger row, so a resumed job never comes back
    with a value that was on its way to being retried.
  - An escalated fan-out item still counts as a failure and still lands in
    `failures.jsonl`. The escalation is *additional* — it is what makes that
    one findable later without re-reading every job.
- `branches = { node = { "route_label" -> target_node; ... }; ... };` —
  conditional dispatch for a branching node's labeled routes. Uncommon
  enough that if you need it, read `crates/cuttlefish-core/src/graph.rs`
  rather than trust a summary here.

## Command

```
cuttlefish build <spec.cuttlefish> [-o <out.cfbundle>]
```

`-o`/`--output` defaults to the spec's own path with its extension set to
`.cfbundle`.

```
$ cuttlefish build examples/summarize.cuttlefish -o /tmp/summarize.cfbundle
checking node `cf_block_echo_summarize`      ... ok  ({path: text} -> {path: text, summary: text})
built: /tmp/summarize.cfbundle  (1 nodes, accepts {path: text}, produces {path: text, summary: text})
```

One `checking node ... ok (signature)` line per pipeline stage, then a
`built: <path> (N nodes, accepts X, produces Y)` summary.

**Link + verify, not compile.** Nothing here compiles Rust or wasm — each
pipeline entry must already be a compiled `.wasm` block or `.cfbundle`. What
`build` does is resolve every entry to bytes, check that each block's
declared output signature is assignable to the next block's declared input
(the "seam" between them), and package the result. Assignability is width
subtyping on record fields — a producer may return extra fields the consumer
ignores, but every field the consumer requires must be present. A seam
mismatch fails the *first* bad seam (not all of them, since downstream
seams' correctness depends on the upstream one holding), names both blocks
and both types, and is wrapped in a "checking the pipeline for `<spec
name>`" context:

```
Error: checking the pipeline for `some_spec`

Caused by:
    block consumer_block expects {chunks: [text]}, but producer_block before it produces {summary: text}.
    Adjust one of the two signatures, or insert a block that converts between them.
```

## Pipeline entries: bare `name@version` resolves through the catalog

A spec's `block = "..."` (or each node's `block = "..."` inside `nodes =
{...}`) entry is resolved the same way in both `cuttlefish build` and
`cuttlefishd`'s `run` path:

- A path that exists relative to the spec's own directory, or that ends in
  `.wasm`/`.cfbundle`, is read directly from disk.
- Anything else — including a bare `name@version` — is looked up in the
  local block catalog (`~/.cuttlefish/catalog`, or `$CUTTLEFISH_HOME/catalog`
  if set; see the `cuttlefish-catalog` skill). A name that was never
  cataloged gives the same not-found-with-suggestion error `catalog show`
  gives, not a raw panic.

So a spec can name a block by its own file, or by whatever it was
`catalog add`ed under — both forms compile.

## Manifest shape

A `.cfbundle` is `CFBD` magic, an 8-byte little-endian manifest length, a
JSON manifest, then the concatenated stage bytes back to back. The manifest
records, per node: name, kind (block or bundle), the resolved
`name@version` if it came from the catalog, its signature, and a byte
offset/length pair *relative to the first byte after the manifest* — never
an absolute file offset, and never found by scanning for `CFBD` (a nested
bundle-as-a-stage starts with that same magic).

## Byte-identical rebuilds

No timestamps or absolute paths appear anywhere in the output. Building the
same spec twice against the same catalog state produces byte-identical
`.cfbundle` files — `cmp` between two builds succeeds. This is what makes
re-cataloging a rebuild not produce a spurious "new" hash for what is really
the same pipeline.

## Refusing to overwrite the source spec

If the output path resolves to the same file as the spec being built (a
spec that already ends in `.cfbundle`, or an explicit `-o` pointing back at
it), `build` refuses rather than clobbering the source:

```
Error: refusing to build: output path examples/summarize.cfbundle is the same file as the spec being built
```

## Verifying end to end

`./try-build.sh` from the repo root builds a spec by direct path, catalogs
that same block and builds a second spec that references it by bare
`name@version`, then rebuilds that second spec and confirms the two
`.cfbundle` outputs are byte-identical — run it to see the whole surface
working before relying on it.
