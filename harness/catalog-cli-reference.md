# `cuttlefish catalog` reference

Hand this file to an agent alongside a task prompt so it knows the exact CLI
surface without needing to read the source first.

## What this is

A local, single-user, content-addressed store mapping `name@version` to a
cataloged wasm block or bundle. Purely local filesystem operations — no
daemon, no network. Lives at `~/.cuttlefish/catalog` by default, or
`$CUTTLEFISH_HOME/catalog` if that env var is set.

## Build

```bash
nix develop --command cargo build -p cuttlefish
```

The binary is then at `./target/debug/cuttlefish`. Always run cargo/the
binary through `nix develop --command ...` in this repo — the system
toolchain is a different, unsupported version.

## Commands

```
cuttlefish catalog add <name>@<version> <path-to-wasm-or-cfbundle>
cuttlefish catalog list
cuttlefish catalog show <name>@<version>
cuttlefish catalog rm <name>@<version>
```

### `add`

Catalogs a compiled `.wasm` block or a `.cfbundle` bundle under an exact
`name@version`. Kind is auto-detected from the file's magic bytes (wasm vs
`CFBD`), never from the file extension.

```
$ cuttlefish catalog add echo-summarize@1 path/to/block.wasm
catalogued echo-summarize@1  ({path: text} -> {path: text, summary: text})
```

If the block has no declared signature (`cf_signature` export missing), it
still catalogs successfully but with a warning:

```
catalogued no-sig@1  (json -> json)
warning: no-sig@1 did not declare a signature (no cf_signature export
present) — cached as the permissive default, which means pipeline::check
will accept it next to almost anything. Add a signature() impl (see
cuttlefish-sdk's Block trait) if this block has a real input/output shape.
```

**Versions are immutable.** Re-`add`ing the same `name@version` fails:

```
$ cuttlefish catalog add echo-summarize@1 path/to/block.wasm
Error: echo-summarize@1 is already catalogued; versions are immutable once published
```

(Exit code 1 in every error case below — this project uses exit code
consistently, not per-error-type codes.)

### `list`

Lists every cataloged entry, one per line, `name@version` then its signature:

```
$ cuttlefish catalog list
echo-summarize@1        {path: text} -> {path: text, summary: text}
```

Empty catalog prints nothing (not an error).

### `show`

Prints one entry's full detail:

```
$ cuttlefish catalog show echo-summarize@1
echo-summarize@1
  kind:      Block
  signature: {path: text} -> {path: text, summary: text}
  hash:      sha256:c3f5c9e7b9aa46620e662106a22ce4085047cc2890cacd865980ed6cb3767055
  created:   2026-08-02T18:03:00Z
```

A miss suggests close-by names (edit distance ≤2), not a bare "not found":

```
$ cuttlefish catalog show ech-summarize@1
Error: no such catalog entry: ech-summarize@1 (did you mean: echo-summarize@1?)
```

### `rm`

Removes the index entry. **Does not free the underlying blob** — no garbage
collection in v1, by design (an orphaned blob is wasted disk space, not a
correctness problem).

```
$ cuttlefish catalog rm echo-summarize@1
removed echo-summarize@1
```

Removing something that doesn't exist is the same `NotFound`-with-suggestion
error as `show`, not a silent no-op.

## Things worth knowing before poking at it

- Names/versions are **case-sensitive, opaque strings** — `v2`, `draft1`,
  `2026-08-02-fixed` are all legal versions, none of them parsed as semver.
- A path ending in `.wasm`, or any existing filesystem path, is used
  directly — this CLI only ever exercises the catalog lookup path when you
  give it a bare `name@version` you've already cataloged.
- Storage on disk: `~/.cuttlefish/catalog/{index.json, index.json.lock,
  blobs/<sha256-hex>}`. `index.json` is safe to read directly if curious —
  it's plain formatted JSON.
- Everything here talks to the filesystem only. No daemon needs to be
  running for any `catalog` subcommand — that's deliberate, unlike `run`/
  `specs` which do need a running `cuttlefishd`.
