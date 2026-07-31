# AGENTS.md

Instructions for AI agents and humans working in this repository.

## The one rule that shapes everything else

**Documentation lives in the code.** Not in `docs/`, not in a wiki, not in a
design doc that drifts out of date while nobody notices.

If you want to explain what a crate does, why a module exists, or why a function
looks the way it does, write it as rustdoc — `//!` at the top of the module,
`///` on the item — and it will be next to the thing it describes, reviewed with
the thing it describes, and rendered to the published API docs automatically.

`docs/` is gitignored. Design specs and implementation plans live there while
work is in flight, and they are working documents, not deliverables. When
something in a working document turns out to be a durable fact about the system,
**move it into a module doc**. Do not commit it as a file.

The reason is not tidiness. A separate design document that contradicts the code
looks authoritative and is wrong, and nothing forces anyone to notice. A module
doc that contradicts the module below it is caught in review, or by
`RUSTDOCFLAGS: -D warnings` when its intra-doc links stop resolving.

## What belongs in module docs

Each crate's `lib.rs` should open with enough context that someone landing on
`docs.rs` from a search engine understands what the crate is for and how it fits
the whole. Specifically:

- **What this crate is responsible for**, in a sentence.
- **The design decisions that would otherwise look arbitrary.** Every crate here
  has at least one. Write down the failure each choice avoids, because that
  failure is invisible from reading the code that resulted.
- **How it relates to its siblings** — link with intra-doc links
  (`[`cuttlefish_host::runner`]`) so broken relationships fail the docs build.

Worked examples of the kind of thing that must be captured this way:

- `cuttlefish-host` drives the guest instead of letting the guest call back into
  the host. That is not a style preference: a single-threaded wasm guest has no
  execution context in which the host could invoke a callback, and wasmtime's
  `Store` is `!Sync` while inference must run on a blocking thread. Someone will
  eventually try to "simplify" this into a blocking host call. The module doc is
  what stops them.
- Blocks receive file *handles* and pull bounded windows, never file contents.
  This keeps guest memory flat regardless of input size, which is what lets the
  project stay on 32-bit wasm.
- The guest returns a pointer to a `Desc { ptr, len }` rather than packing both
  into one `i64`. The packing version is shorter and works — right up until
  pointers are 64 bits.

## Comment conventions

Comment the **why**, never the **what**.

```rust
// Bad — restates the code.
// Increment the counter.
self.seen += 1;

// Bad — addressed to a reviewer, useless once merged.
// Changed this from `<` to `<=` to fix the off-by-one.

// Good — states a constraint the code cannot show.
// Canonicalize both sides: a string prefix check passes for `../` traversal
// and for symlinks pointing outside the granted root.
```

Do not write comments explaining that you changed something, where code came
from, or that a change is correct. Those belong in the commit message and the
pull request, which is where they remain useful. A comment is written for
someone reading this file two years from now who has no idea the change ever
happened.

When you touch code that already carries a rationale comment, **keep it**. If
your change makes it wrong, update it — silently deleting it loses knowledge
that took real effort to acquire.

## Working in this repo

The toolchain is pinned by `flake.nix`. Every build, test, and tooling command
must run inside it:

```console
$ nix develop --command cargo test --workspace
$ nix develop --command cargo clippy --workspace --all-targets -- -D warnings
$ nix develop --command cargo fmt --all
$ nix develop --command cargo llvm-cov --workspace
```

Running `cargo` bare picks up whatever is on `PATH`, and the resulting version
skew produces errors that look like code bugs. This applies to subagents too —
if you dispatch one, tell it the same thing.

Guest processing blocks build for a different target than the host:

```console
$ nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
```

## Testing expectations

Write the failing test first. This matters more here than in most projects
because much of the system is a security boundary — a capability check that
stops checking still returns data and still passes a smoke test. The test is the
enforcement.

Coverage in `crates/cuttlefish-host/src/caps.rs` and `handles.rs` deserves
particular attention: an uncovered branch there is an unenforced rule.

`blocks/` shows as uncovered and that is expected — guest code runs inside wasm,
where the host's instrumentation cannot reach. It is covered behaviourally
through the host's tests.

## Things not to do

- Do not add a `docs/` file and commit it. Put it in a module doc.
- Do not widen the sandbox — a new host command, a new capability kind, a new
  guest import — without an explicit argument for why existing primitives
  cannot do the job.
- Do not bump the MSRV pin in `.github/workflows/ci.yml` without also changing
  `rust-version` in the workspace `Cargo.toml`. They are one decision recorded
  in two places, and moving only the CI pin hides a break from everyone on an
  older compiler.
- Do not rename or restructure `crates/cuttlefish`. It carries the published
  crates.io identity; the workspace grows around it.
