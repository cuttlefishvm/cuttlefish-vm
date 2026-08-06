---
name: cuttlefish-build
description: Use when building, linking, or packaging a .cuttlefish spec into a .cfbundle via the cuttlefish CLI, or when asked about pipeline seam-checking, catalog-name resolution in a spec's pipeline, or reproducible/byte-identical builds
---

# cuttlefish-build

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
- `data_policy` — `Local_only` or `Any`. Discovery metadata for the calling
  agent (pass paths vs. contents); not itself an enforcement mechanism.
- `capabilities = [ Read "path", ... ]` — the only supported capability kind
  is `Read`; every path becomes a root a block may read beneath. This is the
  actual enforcement boundary, checked at build time and again at runtime.
- `block = "..."` — sugar for a one-node graph. A path relative to the
  spec's own directory (or one ending `.wasm`/`.cfbundle`) reads straight
  from disk; anything else is a catalog `name@version` lookup (see
  `cuttlefish-catalog`).
- `nodes = { name = { block = "..."; in = { field = other_node.out; }; };
  ... };` — the real multi-block graph `block = "..."` desugars to. Each
  node's `in` maps its input record's fields to `other_node.out`
  expressions; omit `in` for a node with no inbound edge. Typechecked the
  same way as any other seam (see "Command" below) before anything runs.
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
