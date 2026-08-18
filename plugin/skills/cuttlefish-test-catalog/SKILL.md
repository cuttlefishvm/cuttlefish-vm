---
name: cuttlefish-test-catalog
description: Use when independently testing or adversarially verifying Cuttlefish catalog add, list, show, remove, immutability, corruption handling, or concurrent writers
---

# Test the Cuttlefish catalog

Test `cuttlefish catalog add/list/show/rm` as a black box. Use
`cuttlefish-catalog` for exact syntax and output shapes. Do not read the
Rust source to determine how to drive the CLI.

## Safety and setup

Record a read-only digest or absence marker for the default catalog without
creating it. Create one validated temporary root, immediately set
`CUTTLEFISH_HOME` beneath it, and install an EXIT trap that deletes only
that resolved root. Assert every test index, lock, and blob appears only
beneath `$CUTTLEFISH_HOME/catalog`.

Build through the pinned toolchain:

```console
nix develop --command cargo build -p cuttlefish
nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
```

Resolve the repository root and invoke its absolute
`$repo/target/debug/cuttlefish` binary. Use the freshly built example wasm
as the signed fixture. Under the temporary root, construct these byte-exact
fixtures:

- unsigned valid wasm: `00 61 73 6d 01 00 00 00`;
- different unsigned valid wasm with an empty custom section:
  `00 61 73 6d 01 00 00 00 00 02 01 78`;
- separately malformed wasm that begins with valid magic but has a
  truncated or invalid section.

## Acceptance matrix

1. Add the signed block, list it, show it, and remove it. Require `list` to
   contain exactly that entry and signature. Check its kind, exact
   signature, and SHA-256 digest. Confirm the final live list is empty.

2. Add the same live `name@version` twice: once with identical bytes and
   once with different valid bytes. Both must fail without changing the
   original entry. Remove it, then require re-adding the same bytes to
   succeed with the original hash. Remove it again and require different
   bytes to fail with both the retired and attempted hashes in the
   diagnostic while leaving the identity retired.

3. Look up and remove a close miss and require a useful suggestion. A far
   miss must fail without a nonsensical suggestion. Removing a missing or
   already removed entry must fail rather than silently succeed. Require
   the isolated index and blob digests to remain unchanged after each
   missing-entry or suggestion probe.

4. Try a plain file and a magic-looking but invalid wasm. Both must fail
   without a new entry or published blob. Catalog a valid wasm with no
   declared signature and require both the permissive `json -> json`
   default and an explicit warning.

5. Replace only the isolated `index.json` with truncated JSON, a valid JSON
   value of the wrong shape, and an unsupported version. For each case,
   `list`, `show`, `add`, and `rm` must fail loudly as corrupt, never treat
   the catalog as empty, never panic, and never overwrite the corrupt
   bytes. Restore the known-good index between cases.

6. Use a barrier or start gate plus bounded waits to overlap at least eight
   adds with different names in the same isolated catalog. Require every
   child exit code, the exact final key/hash set with no lost updates, a
   parseable index, and no leftover temporary index. Also overlap
   contenders for one key while alternating the two valid unsigned
   fixtures: exactly one may win, its stored hash must equal one complete
   contender, and losers must report immutability or already-exists rather
   than corruption or I/O failure.

7. Remove every live test entry, require an empty list, and compare the
   real default catalog with its original digest or absence marker. Account
   for unrelated concurrent writers when interpreting a mismatch. Run the
   cleanup trap and verify the temporary root is gone.

## Report contract

For every probe, report the command or action, expected behavior, actual
behavior and exit code, and whether they matched. For concurrent work,
include evidence that execution overlapped and list every child exit code.
Call out confusing diagnostics and unexpected success or failure.

Do not modify product code. Report bugs with a minimal reproduction rather
than fixing them.
