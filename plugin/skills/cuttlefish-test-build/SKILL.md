---
name: cuttlefish-test-build
description: Use when independently testing or adversarially verifying Cuttlefish bundle builds, catalog resolution, pipeline seam checks, reproducibility, and source-clobber protection
---

# Test Cuttlefish bundle builds

Test `cuttlefish build` as a black box. Use `cuttlefish-build` for build
syntax and output shapes, and `cuttlefish-catalog` for catalog operations.
Do not read the Rust source to determine how to drive the CLI.

## Safety and setup

Record a read-only digest or absence marker for the default catalog without
creating it. Then create one validated temporary root, set
`CUTTLEFISH_HOME` beneath it before running Cuttlefish, and install an EXIT
trap that deletes only that resolved root. Never touch the real catalog.

Build with the pinned toolchain:

```console
nix develop --command cargo build -p cuttlefish
nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
```

Resolve the repository root first and invoke its absolute
`$repo/target/debug/cuttlefish` path, not an older binary from `PATH`. Keep
every spec, catalog entry, and output under the temporary root, and assert
catalog artifacts appear only beneath `$CUTTLEFISH_HOME/catalog`.

## Acceptance matrix

1. Build `examples/summarize.cuttlefish` with `-o`. Confirm the output
   exists, begins with bundle magic, and reports each checked node plus the
   final `built:` summary.

2. Catalog compatible blocks and build one pipeline containing at least
   two bare `name@version` stages. Inspect the bundle manifest and require
   every node's exact catalog `resolved` value and order, proving the
   entries were not treated as paths.

3. Use that same catalog-resolved multi-stage spec for builds to different
   output paths, from different working directories, with a delay between
   runs. Require byte-identical bundles and matching hashes; timestamps
   and absolute paths must not leak into the manifest.

4. Catalog a built `.cfbundle`, reference its `name@version` as the only
   stage of a second spec, and build successfully. Require the outer
   manifest node to report bundle kind, the exact resolved catalog name,
   and the inner bundle's compact signature. Use its offset and length to
   prove the embedded bytes equal the original bundle; do not scan for a
   second magic header.

5. Reference a missing catalog entry. Require a clear not-found error and
   a nearby-name suggestion when one exists, with no panic or output file.

6. Construct a real seam mismatch using two known signatures, such as
   wrapping the producer's record output inside a consumer field that
   expects text. Require an error naming both blocks and both types, not a
   generic type error, and require that no partial output exists.

7. Pass a missing spec, a directory, malformed syntax, invalid UTF-8, an
   unknown field, and a missing block path. Each must fail with path or
   parse context, no panic, and no partial bundle.

8. Try to write output over the input spec using the exact path, a lexical
   alias, a symlink, and a hard link. Also test default output for a valid
   spec whose filename already ends in `.cfbundle`. Use only disposable
   copies. Each must fail, preserve the source bytes, and produce no
   success lines.

9. Remove all temporary catalog entries, confirm the isolated catalog is
   empty, and compare the real default catalog with its original digest or
   absence marker. Account for unrelated concurrent writers when
   interpreting a mismatch. Run cleanup and verify the temporary root is
   gone.

## Report contract

For every case, report the command or action, expected behavior, actual
behavior and exit code, and whether they matched. Call out confusing
diagnostics and unexpected success or failure. If all cases pass, name the
adversarial probes that could plausibly have exposed a defect.

Do not modify product code. Report bugs with a minimal reproduction rather
than fixing them.
