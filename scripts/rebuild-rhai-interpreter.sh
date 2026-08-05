#!/usr/bin/env bash
# Regenerates crates/cuttlefish-host/assets/rhai-interpreter.wasm from
# blocks/rhai-interpreter's source. Run this by hand whenever that source
# changes, then commit the updated asset (and the .source-sha256 sidecar
# below) alongside your source change — there is no build.rs doing this
# automatically (a build.rs shelling out a recursive `cargo build` is
# fragile: target-dir locking, no clean way to bound recursion), and the
# asset must already exist in git before `cuttlefish`/`cuttlefishd` can
# `include_bytes!` it — see the checked-in asset's own comment in lib.rs
# for why.
#
# Also writes crates/cuttlefish-host/assets/rhai-interpreter.wasm.source-sha256
# — a hash of blocks/rhai-interpreter's own source files (not the compiled
# wasm). CI checks THIS, not the wasm bytes: a wasm build is not
# byte-reproducible across different rustc/LLVM versions (confirmed for
# real — this script runs through this repo's pinned `nix develop`
# toolchain, while CI's other jobs all use `dtolnay/rust-toolchain@stable`,
# a materially different, drifting toolchain; the same source produced
# different wasm bytes on the two, even after trying --remap-path-prefix
# to rule out embedded-absolute-path drift as the cause). Comparing a
# source hash instead sidesteps toolchain-reproducibility entirely — it
# only asks "does the checked-in wasm still correspond to this exact
# source," which a hash answers without ever needing to reproduce the
# compiler's output bit-for-bit.
set -euo pipefail
cd "$(dirname "$0")/.."

nix develop --command cargo build --release -p cf-block-rhai-interpreter --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/cf_block_rhai_interpreter.wasm \
   crates/cuttlefish-host/assets/rhai-interpreter.wasm

# Sorted so the hash doesn't depend on filesystem iteration order.
find blocks/rhai-interpreter -type f \( -name '*.toml' -o -name '*.rs' \) | sort \
  | xargs cat \
  | shasum -a 256 \
  | cut -d' ' -f1 \
  > crates/cuttlefish-host/assets/rhai-interpreter.wasm.source-sha256

echo "regenerated crates/cuttlefish-host/assets/rhai-interpreter.wasm (+ .source-sha256)"
