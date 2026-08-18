---
name: cuttlefish-catalog
description: Use when cataloging, listing, showing, or removing a wasm block or bundle via the cuttlefish CLI, or when asked to work with a name@version entry in the cuttlefish block catalog
---

# cuttlefish-catalog

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

The local, single-user, content-addressed store mapping `name@version` to a
cataloged wasm block or bundle, reachable via `cuttlefish catalog`. Purely
local filesystem operations — no daemon, no network. Lives at
`~/.cuttlefish/catalog` by default, or `$CUTTLEFISH_HOME/catalog` if that env
var is set.

## Build

**REQUIRED SUB-SKILL:** Use cuttlefish-cli to get the `cuttlefish` binary
before proceeding.

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

The identifier must be a well-formed `name@version`: exactly one `@`, both
halves non-empty, and each limited to letters, digits, `.`, `-` and `_`.
Anything else is rejected (exit 1) rather than catalogued under a junk key —
notably a dropped `@version`, which is otherwise an easy typo to make:

```
$ cuttlefish catalog add echo-summarize block.wasm
Error: "echo-summarize" is not a name@version (expected <name>@<version>, e.g. echo-summarize@1)
```

**Versions are immutable.** Re-`add`ing the same `name@version` fails
(exit 1):

```
Error: echo-summarize@1 is already catalogued; versions are immutable once published
```

Removing an entry does **not** release its identity. A `name@version` that
was published and then `rm`'d can be re-added with the *same* bytes — that's
an undo of the removal — but re-adding it with *different* content is
rejected, so `rm` is not a way to quietly republish a version someone may
already depend on:

```
$ cuttlefish catalog rm thing@1
removed thing@1
$ cuttlefish catalog add thing@1 the-same-block.wasm
catalogued thing@1  (json -> json)
$ cuttlefish catalog rm thing@1 && cuttlefish catalog add thing@1 a-different-block.wasm
Error: thing@1 was previously catalogued with different content; versions are
immutable once published (sha256:93a44b... -> sha256:ce9e2d...)
```

### `list`

One line per entry, `name@version` then signature, separated by at least two
spaces (names are padded to a column, but a long name never runs into its
signature). Empty catalog prints nothing.

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
no-op. The removed `name@version` is recorded under `retired` in
`index.json`, which is what lets a later `add` tell an undo from a
republish (see `add` above).

## Things worth knowing

- Names/versions are **case-sensitive, opaque strings** — not parsed as
  semver. `1.2.3-rc.1` is a legal version because those characters are
  allowed, not because anything understands it as semver.
- Validation applies to `add` only. `show` and `rm` stay permissive, so a
  junk key already in an index (hand-edited, or written before validation
  existed) is still inspectable and removable rather than stranded.
- A path ending in `.wasm`, or any existing filesystem path, is used
  directly — the catalog lookup only fires for a bare `name@version`.
- On-disk layout: `~/.cuttlefish/catalog/{index.json, index.json.lock,
  blobs/<sha256-hex>}`. `index.json` is plain formatted JSON, safe to read
  directly; it holds `entries` (live) and `retired` (removed
  `name@version` -> the hash it was published with).
- No daemon needs to be running for any `catalog` subcommand — unlike
  `cuttlefish run`/`specs`, which need a live `cuttlefishd`.

## Verifying end to end

`./try-catalog.sh` from the repo root runs a full add/list/show/duplicate-add/
typo-lookup/rm cycle against the real `blocks/echo-summarize` example block —
run it to see the whole surface working before relying on it.
