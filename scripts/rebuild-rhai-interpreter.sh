#!/usr/bin/env bash
# Regenerates crates/cuttlefish-host/assets/rhai-interpreter.wasm from
# blocks/rhai-interpreter's source. Run this by hand whenever that source
# changes, then commit the updated asset alongside your source change —
# there is no build.rs doing this automatically (a build.rs shelling out a
# recursive `cargo build` is fragile: target-dir locking, no clean way to
# bound recursion), and the asset must already exist in git before
# `cuttlefish`/`cuttlefishd` can `include_bytes!` it — see the checked-in
# asset's own comment in lib.rs for why. CI independently re-runs this
# script and diffs the result against what's checked in, so a forgotten
# regeneration fails loudly rather than silently shipping a stale
# interpreter.
set -euo pipefail
cd "$(dirname "$0")/.."

# Without --remap-path-prefix, rustc embeds this checkout's absolute path
# (in panic-location strings) into the compiled output, so the exact same
# source produces DIFFERENT bytes depending on where it was checked out —
# confirmed for real: CI independently rebuilds and byte-compares this
# asset (see .github/workflows/ci.yml), and it failed the very first time
# because CI's checkout path (/home/runner/work/...) is never the same as
# whoever ran this script locally. Remapping this checkout's real path to
# a fixed placeholder makes the two builds' embedded paths — and so their
# bytes — identical regardless of where each one actually ran.
#
# This RUSTFLAGS setting fully replaces (does not merge with)
# .cargo/config.toml's own `[target.wasm32-unknown-unknown] rustflags`
# (the getrandom_backend cfg flag rhai needs to compile at all), so both
# flags are repeated here explicitly. Keep this in sync with the identical
# RUSTFLAGS line in .github/workflows/ci.yml's "Rhai interpreter asset is
# up to date" job if either flag set ever changes.
export RUSTFLAGS='--cfg getrandom_backend="custom" --remap-path-prefix='"$(pwd)"'=/cuttlefishvm'
nix develop --command cargo build --release -p cf-block-rhai-interpreter --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/cf_block_rhai_interpreter.wasm \
   crates/cuttlefish-host/assets/rhai-interpreter.wasm
echo "regenerated crates/cuttlefish-host/assets/rhai-interpreter.wasm"
