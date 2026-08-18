#!/usr/bin/env bash
# Download prebuilt cuttlefish binaries for this platform.
#
# Shipped as a release asset so nobody — human or agent — has to write this
# script before they can use it. Fetching and running a file is one command;
# authoring one is an editing step that needs approval, gets retyped subtly
# differently each time, and is exactly the friction that had agents give up
# at "cuttlefish: command not found".
#
#   curl -fsSL https://github.com/cuttlefishvm/cuttlefish-vm/releases/latest/download/install.sh -o /tmp/cf-install.sh
#   bash /tmp/cf-install.sh
#
# Prints TAG= and BIN_DIR= on success. Add BIN_DIR to PATH.
#
# Deliberately not a `curl | bash` one-liner: piping a download straight
# into a shell runs whatever arrives, including a truncated file, with no
# chance to look first. Two commands cost nothing and leave the script on
# disk to inspect.
set -euo pipefail

REPO="${CUTTLEFISH_REPO:-cuttlefishvm/cuttlefish-vm}"
API="https://api.github.com/repos/$REPO/releases"

# A specific tag can be pinned; otherwise take whatever is current. Pinning
# matters for reproducing an old run, but the default is the latest release
# — a stale binary reproduces bugs that were fixed releases ago.
TAG="${CUTTLEFISH_TAG:-}"
if [ -z "$TAG" ]; then
  TAG=$(curl -fsSL "$API/latest" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
fi
if [ -z "$TAG" ]; then
  echo "could not determine the latest release tag from $API/latest" >&2
  exit 1
fi
VERSION="${TAG#v}"
CACHE_DIR="${CUTTLEFISH_CACHE:-$HOME/.cache/cuttlefish/bin}/$TAG"

# Already unpacked for *this* tag. Note the tag is resolved first: a cache
# hit means "the current release is present", not "something is present".
if [ -x "$CACHE_DIR/cuttlefish" ] && [ -x "$CACHE_DIR/cuttlefishd" ]; then
  echo "TAG=$TAG"
  echo "BIN_DIR=$CACHE_DIR"
  exit 0
fi

case "$(uname -s)" in
  Linux)
    case "$(uname -m)" in
      aarch64|arm64) TARGET=aarch64-unknown-linux-gnu ;;
      *)             TARGET=x86_64-unknown-linux-gnu ;;
    esac
    EXT=tar.gz
    ;;
  Darwin)
    case "$(uname -m)" in
      arm64|aarch64) TARGET=aarch64-apple-darwin ;;
      *)             TARGET=x86_64-apple-darwin ;;
    esac
    EXT=tar.gz
    ;;
  *)
    echo "no prebuilt binary for $(uname -s)/$(uname -m)." >&2
    echo "Windows: download the x86_64-pc-windows-msvc .zip from" >&2
    echo "  https://github.com/$REPO/releases/$TAG" >&2
    echo "Otherwise build from source: cargo install cuttlefish cuttlefishd" >&2
    exit 1
    ;;
esac

ASSET="cuttlefish-${VERSION}-${TARGET}.${EXT}"
BASE="https://github.com/$REPO/releases/download/$TAG"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

curl -fsSL "$BASE/$ASSET" -o "$ASSET" || {
  echo "no asset $ASSET in release $TAG — see https://github.com/$REPO/releases/$TAG" >&2
  exit 1
}
curl -fsSL "$BASE/SHA256SUMS" -o SHA256SUMS || {
  echo "release $TAG has no SHA256SUMS; refusing to install unverified binaries" >&2
  exit 1
}

# A checksum mismatch is a hard stop. `set -e` makes the failure fatal, and
# that is the point: falling back to an unverified binary would defeat the
# only integrity check in this path.
if command -v sha256sum >/dev/null 2>&1; then
  grep " ${ASSET}\$" SHA256SUMS | sha256sum -c -
else
  grep " ${ASSET}\$" SHA256SUMS | shasum -a 256 -c -
fi

mkdir -p "$CACHE_DIR"
tar xzf "$ASSET" -C "$CACHE_DIR" --strip-components=1

# Verify what was actually extracted rather than trusting the archive's
# shape: a layout change upstream would otherwise leave an empty directory
# that later reads as a cache hit.
for bin in cuttlefish cuttlefishd; do
  if [ ! -x "$CACHE_DIR/$bin" ]; then
    echo "extracted $ASSET but $bin is missing from $CACHE_DIR" >&2
    exit 1
  fi
done

echo "TAG=$TAG"
echo "BIN_DIR=$CACHE_DIR"
