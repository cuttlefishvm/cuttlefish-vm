---
name: cuttlefish-catalog
description: Use when cataloging, listing, showing, or removing a wasm block or bundle via the cuttlefish CLI, or when asked to work with a name@version entry in the cuttlefish block catalog
---

# cuttlefish-catalog

## Overview

The local, single-user, content-addressed store mapping `name@version` to a
cataloged wasm block or bundle, reachable via `cuttlefish catalog`. Purely
local filesystem operations — no daemon, no network. Lives at
`~/.cuttlefish/catalog` by default, or `$CUTTLEFISH_HOME/catalog` if that env
var is set.

## Build

```bash
nix develop --command cargo build -p cuttlefish
```

Binary lands at `./target/debug/cuttlefish`. Always run cargo/the binary
through `nix develop --command ...` in this repo — the system toolchain is a
different, unsupported version.

## Commands

```
cuttlefish catalog add <name>@<version> <path-to-wasm-or-cfbundle>
cuttlefish catalog list
cuttlefish catalog show <name>@<version>
cuttlefish catalog rm <name>@<version>
```

### `add`

Kind (block vs. bundle) is auto-detected from magic bytes, never the file
extension.

```
$ cuttlefish catalog add echo-summarize@1 path/to/block.wasm
catalogued echo-summarize@1  ({path: text} -> {path: text, summary: text})
```

A block with no declared signature still catalogs, but warns:

```
catalogued no-sig@1  (json -> json)
warning: no-sig@1 did not declare a signature (no cf_signature export
present) — cached as the permissive default...
```

**Versions are immutable.** Re-`add`ing the same `name@version` fails
(exit 1):

```
Error: echo-summarize@1 is already catalogued; versions are immutable once published
```

### `list`

One line per entry, `name@version` then signature. Empty catalog prints
nothing.

### `show`

Full detail (kind, signature, hash, created). A miss suggests close-by
names, not a bare "not found":

```
Error: no such catalog entry: ech-summarize@1 (did you mean: echo-summarize@1?)
```

### `rm`

Removes the index entry only — **does not free the underlying blob** (no
garbage collection in v1, by design). Removing something that doesn't exist
is the same not-found-with-suggestion error `show` gives, not a silent
no-op.

## Things worth knowing

- Names/versions are **case-sensitive, opaque strings** — not parsed as
  semver.
- A path ending in `.wasm`, or any existing filesystem path, is used
  directly — the catalog lookup only fires for a bare `name@version`.
- On-disk layout: `~/.cuttlefish/catalog/{index.json, index.json.lock,
  blobs/<sha256-hex>}`. `index.json` is plain formatted JSON, safe to read
  directly.
- No daemon needs to be running for any `catalog` subcommand — unlike
  `cuttlefish run`/`specs`, which need a live `cuttlefishd`.

## Verifying end to end

`./try-catalog.sh` from the repo root runs a full add/list/show/duplicate-add/
typo-lookup/rm cycle against the real `blocks/echo-summarize` example block —
run it to see the whole surface working before relying on it.
