---
name: cuttlefish-test-author
description: Use when independently testing or adversarially verifying Cuttlefish block authoring, including Rhai and Rust scaffolds, invalid inputs, cataloging, and real job execution
---

# Test Cuttlefish block authoring

Test the `cuttlefish-author` flow as a black box. Use the
`cuttlefish-author` skill for authoring syntax and `cuttlefish-run` for
daemon lifecycle and job commands. Do not read the Rust source to
determine how to drive either flow.

Independently verify the documented behavior, then try to break it. Do not
just re-run `./try-author.sh`.

## Test procedure

1. Create one validated temporary root and set `CUTTLEFISH_HOME` to a
   directory beneath it before starting any Cuttlefish process. Install an
   EXIT trap that shuts down started daemons and deletes only when its
   resolved target equals that validated root. Bound daemon startup, job
   polling, and replay tests with explicit timeouts.

2. Build with `nix develop --command cargo build -p cuttlefish -p
   cuttlefishd`, as required by the repository toolchain. Run every Rust
   build through `nix develop` and target `wasm32-unknown-unknown`. Invoke
   the freshly built absolute `target/debug/cuttlefish` and
   `target/debug/cuttlefishd` paths, or prepend that directory to `PATH` and
   prove command resolution points there; never test an older installed
   binary accidentally.

3. From a clean temporary project with no `.cuttlefish/` directory,
   exercise both languages:

   - Scaffold the default Rhai block. Confirm it contains exactly one
     `block.rhai` with the declared signature and identity expression.
     Catalog it, then verify `list` and `show` report Script kind and the
     exact signature.
   - Start a daemon against a spec referencing the script, submit a job,
     and confirm the identity result. Edit the script and confirm a direct
     `.rhai` path picks up the change on the next run without cataloging a
     new version.
   - Change the script to call `infer(...)` and use a `Stub` model so the
     host round-trip is deterministic and needs no external model service.
   - Scaffold a Rust block with `--lang rust`. Confirm it contains
     `Cargo.toml` and `src/lib.rs`, with the expected crate name, `cdylib`,
     SDK version, signature, and identity implementation. Build it, verify
     the output begins with wasm magic bytes, catalog it, and confirm it
     runs. If the generated SDK version is not published yet, pass a Cargo
     patch to an external config outside the scaffold, pointing to this
     checkout's `cuttlefish-sdk`. Snapshot the pristine scaffold first and
     exclude only that external build plumbing from shape assertions.

4. Exercise every adversarial case below:

   - Omit `--input`, omit `--output`, and pass an unsupported `--lang`.
     Each must fail without leaving partial output.
   - Scaffold a name that already exists using changed types and again
     using a changed language. Both attempts must fail, and a byte/tree
     comparison must prove the original was not overwritten.
   - Reject names that begin with a digit, contain `.`, contain path
     separators or traversal, or match a Windows-reserved device name such
     as `con`, `aux`, or `nul`, ignoring case. Confirm no files appear
     outside the intended scaffold root.
   - Reject unparseable `--input` and `--output` types with a clear parse
     error, not a panic or stack trace, and no partial scaffold.
   - Give a Rust description containing a quote, backslash, and newline;
     the generated TOML must remain valid and the crate must build.
   - Reject a `.rhai` file with no `//! signature: ...` header at catalog
     time.
   - Reject a `.rhai` file whose signature header does not parse.
   - Reject a `.rhai` file containing two signature headers as ambiguous.
   - Wrap `infer(...)` in `try { } catch { }`, make the catch body change
     control flow before a later host call, and confirm within the timeout
     that replay fails loudly with `nondeterministic_replay`.
   - Build a spec containing a Script-kind node. It must fail and name the
     node rather than silently embedding an unusable bundle.
   - In two temporary projects with separate `CUTTLEFISH_HOME` values,
     overlap scaffold and catalog operations using the same name and
     version but different content. Confirm each isolated catalog contains
     only its own expected hash and signature.

5. Remove scaffold source directories after cataloging and confirm their
   content-addressed catalog entries remain readable. Remove every test
   entry, confirm the catalog is empty, stop all daemons, remove the
   temporary root, and verify cleanup completed.

## Report contract

For every case, report:

- the command or action;
- the expected behavior;
- the actual behavior and exit code;
- whether they matched.

Call out confusing diagnostics and unexpected success or failure. If all
tests match expectations, name the adversarial cases that could plausibly
have exposed a defect.

Do not modify product code. If a real bug appears, report a reproducible
command, expected behavior, actual behavior, and exit code instead of
fixing it.
