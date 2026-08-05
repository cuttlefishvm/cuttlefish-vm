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

nix develop --command cargo build --release -p cf-block-rhai-interpreter --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/cf_block_rhai_interpreter.wasm \
   crates/cuttlefish-host/assets/rhai-interpreter.wasm
echo "regenerated crates/cuttlefish-host/assets/rhai-interpreter.wasm"
