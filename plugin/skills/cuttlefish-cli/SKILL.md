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

Run through in order; stop at the first that applies.

### 1. Already resolvable

- `cuttlefish`/`cuttlefishd` already on `PATH`? Use them, done.
- Otherwise, check whether `~/.cache/cuttlefish/bin/<tag>/` already holds
  both binaries for the tag step 3 below would fetch anyway (cheap to
  check the tag first via the same `curl`/`python3` step 3 uses, before
  downloading anything) — if so, use the cached copy, done.

### 2. Actively editing this checkout? Build from source.

```bash
root=$(git rev-parse --show-toplevel 2>/dev/null) || root=""
if [ -n "$root" ] && [ -f "$root/crates/cuttlefish/Cargo.toml" ] && \
   [ -n "$(git -C "$root" status --porcelain -- crates blocks)" ]; then
  echo "dirty cuttlefish-vm checkout — build from source"
fi
```

If that prints — this is a `cuttlefish-vm` checkout with uncommitted
changes touching `crates/` or `blocks/` — a released binary can't reflect
those edits, so build from source instead:

```bash
nix develop --command cargo build -p cuttlefish -p cuttlefishd
```

Binaries land at `./target/debug/{cuttlefish,cuttlefishd}`. Always run
cargo/the binaries through `nix develop --command ...` in this repo — the
system toolchain is a different, unsupported version.

### 3. Otherwise: download the latest release

```bash
curl -fsSL https://api.github.com/repos/cuttlefishvm/cuttlefish-vm/releases/latest \
  -o /tmp/cuttlefish-release.json
```

Determine the tag, the target asset name, and its download URL with
`python3` (matching this repo's existing convention — `release.yml`'s own
tag-verification step and `scripts/rebuild-rhai-interpreter.sh` both
already lean on `python3` rather than assuming `jq` is installed):

```bash
python3 - <<'PY'
import json, platform, sys

with open("/tmp/cuttlefish-release.json") as f:
    release = json.load(f)

tag = release["tag_name"]
version = tag[1:] if tag.startswith("v") else tag  # release assets have no "v" prefix

system = platform.system()
machine = platform.machine().lower()
if system == "Linux":
    target = "aarch64-unknown-linux-gnu" if machine in ("aarch64", "arm64") else "x86_64-unknown-linux-gnu"
    ext = "tar.gz"
elif system == "Darwin":
    target = "aarch64-apple-darwin" if machine in ("aarch64", "arm64") else "x86_64-apple-darwin"
    ext = "tar.gz"
elif system == "Windows":
    target = "x86_64-pc-windows-msvc"
    ext = "zip"
else:
    sys.exit(f"unsupported platform: {system}/{machine}")

asset = f"cuttlefish-{version}-{target}.{ext}"
urls = {a["name"]: a["browser_download_url"] for a in release["assets"]}

if asset not in urls or "SHA256SUMS" not in urls:
    sys.exit(f"expected assets not found in release {tag}: need {asset} and SHA256SUMS, got {sorted(urls)}")

print(f"TAG={tag}")
print(f"ASSET={asset}")
print(f"ASSET_URL={urls[asset]}")
print(f"SUMS_URL={urls['SHA256SUMS']}")
PY
```

Capture that script's four `KEY=value` lines into shell variables (e.g.
`eval "$(python3 ... )"`), then download and verify — a checksum mismatch
is a hard stop, never fall back to running an unverified binary:

```bash
mkdir -p /tmp/cuttlefish-dl && cd /tmp/cuttlefish-dl
curl -fsSL "$ASSET_URL" -o "$ASSET"
curl -fsSL "$SUMS_URL" -o SHA256SUMS
shasum -a 256 -c <(grep " ${ASSET}\$" SHA256SUMS)
```

(Windows: download the same way, then `Get-FileHash "$ASSET" -Algorithm
SHA256` compared by hand against `SHA256SUMS`'s matching line.)

Extract into the version-keyed cache, so a later invocation for the same
tag reuses it without re-downloading. The archive contains one top-level
directory (`cuttlefish-<version>-<target>/`), hence `--strip-components=1`:

```bash
mkdir -p ~/.cache/cuttlefish/bin/"$TAG"
tar xzf "$ASSET" -C ~/.cache/cuttlefish/bin/"$TAG" --strip-components=1
```

(Windows `.zip`: `Expand-Archive`, then move the one extracted
subdirectory's contents up into `~/.cache/cuttlefish/bin/$TAG/`.)

Binaries are now at `~/.cache/cuttlefish/bin/$TAG/{cuttlefish,cuttlefishd}`
(`.exe` on Windows).

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
