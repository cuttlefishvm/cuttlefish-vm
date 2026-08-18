---
name: cuttlefish-cli
description: Use when any other cuttlefish-* skill needs the cuttlefish or cuttlefishd binaries — resolves PATH, then a cached download, then a source build (only if actively editing this checkout), then a checksum-verified download from the latest GitHub Release
---

# cuttlefish-cli

## Overview

If you're dispatching other cuttlefish work as an agent anyway (see
`cuttlefish-runner`/`cuttlefish-block-author`), dispatch `cuttlefish-
binary-resolver` first instead of following the procedure below inline —
it's the same steps, already baked into that agent's system prompt, and
keeps the resolution noise out of the calling session's transcript. This
skill is the procedure itself, for when you're driving things inline
(or when writing/maintaining that agent).

One shared procedure every other `cuttlefish-*` skill points at for "how do
I get a `cuttlefish`/`cuttlefishd` binary," instead of each saying it
independently. Prefers a checksum-verified GitHub Release download over
building from source — most callers just want to *use* cuttlefish, not
build it.

## Resolving a binary

**If `cuttlefish` is not on `PATH`, download it from GitHub Releases.** That
is the answer in almost every case, and it is one command — the script in
"Download the release" below. Run it. It resolves the latest tag, verifies a
checksum, extracts to `~/.cache/cuttlefish/bin/<tag>/`, and prints the
directory to put on `PATH`.

Do not stop at "not found on PATH" and report that binaries are unavailable.
They are downloadable, always, on every supported platform.

Two things worth knowing before you run it:

- **It caches by tag and re-running is nearly free.** A second run finds the
  tag directory already populated and exits immediately. So there is no
  reason to avoid it, and no reason to hunt through `~/.cache` by hand
  first — the script does that lookup itself, against the *current* release
  rather than whatever happens to be lying around.
- **Never reuse an old cached tag without checking.** A `~/.cache/cuttlefish/bin/v0.0.7`
  left by a previous session is not "close enough": releases carry fixes
  that change behaviour, and silently running a stale binary produces bugs
  that were fixed months ago. The script compares against the real latest
  tag every time, which costs one API call.

### Building from source instead

Only when this *is* a `cuttlefish-vm` checkout with uncommitted changes
under `crates/` or `blocks/` — a released binary cannot reflect edits that
are not in a release:

```bash
root=$(git rev-parse --show-toplevel 2>/dev/null) || root=""
if [ -n "$root" ] && [ -f "$root/crates/cuttlefish/Cargo.toml" ] && \
   [ -n "$(git -C "$root" status --porcelain -- crates blocks)" ]; then
  nix develop --command cargo build -p cuttlefish -p cuttlefishd
  # binaries at ./target/debug/{cuttlefish,cuttlefishd}
fi
```

Always run cargo through `nix develop --command ...` in this repo — the
system toolchain is a different, unsupported version.

Anywhere else — an empty project directory, someone else's repo, a
scratch dir — download. Do not build from source just because a checkout
happens to be nearby.

### Download the release

```bash
curl -fsSL https://github.com/cuttlefishvm/cuttlefish-vm/releases/latest/download/install.sh \
  -o /tmp/cf-install.sh
bash /tmp/cf-install.sh
```

It prints `TAG=` and `BIN_DIR=`; put `BIN_DIR` on `PATH`. It resolves the
latest tag, verifies the archive against the release's `SHA256SUMS`, and
caches under `~/.cache/cuttlefish/bin/<tag>/`, so a second run exits
immediately.

**Do not write this script yourself.** It ships as a release asset
precisely so nobody has to: an authored file needs approval, gets retyped
slightly differently each time, and is the friction that had agents stop at
"cuttlefish: command not found".

`CUTTLEFISH_TAG=v0.0.8` pins an older release when reproducing a past run.
Windows has no installer — take the `x86_64-pc-windows-msvc` zip from the
release page.

## Things worth knowing

- `test-*.md` commands (`test-run`, `test-build`, `test-catalog`,
  `test-author`) never go through this skill — they adversarially test
  *this checkout's* code, so they always build from source directly
  (`nix develop --command cargo build -p cuttlefish -p cuttlefishd`),
  regardless of whether the checkout has uncommitted changes.
- `SHA256SUMS` covers integrity against transport corruption and this
  repo's own release process, not provenance the way code-signing would —
  a checksum match means "this is the file the release actually
  published," not an independent guarantee of what's inside it.
- The cache is keyed by release tag, not overwritten — two different
  pinned versions can coexist under `~/.cache/cuttlefish/bin/`.
