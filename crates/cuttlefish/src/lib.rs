//! Native tooling for AI agents: a local runtime for jobs an agent hands off.
//!
//! A coding agent delegates a job — summarize this, classify that, extract
//! these fields — and Cuttlefish runs it on this machine against a local
//! model, returning a structured result. Two things make that worth doing:
//!
//! - **Data stays local.** For jobs marked local-only, the calling agent
//!   passes *paths*, not contents. Cuttlefish opens the files itself, so
//!   proprietary source and personal data never enter a frontier model's
//!   context at all. The guarantee is not "we also ran something locally" — it
//!   is that the remote model never received the bytes.
//! - **Frontier prices stop applying to grunt work.** Bulk, repetitive, and
//!   mechanical subtasks do not need a trillion-parameter model, and offloading
//!   them keeps tokens and context for the work that does.
//!
//! The limitation is worth stating plainly rather than discovering later: you
//! cannot run a frontier-class model on a laptop. Cuttlefish targets the large
//! subset of agent work that does not need frontier reasoning. It is not a
//! replacement for one.
//!
//! # How a job runs
//!
//! A job is described by a `.cuttlefish` spec — which model, what the job may
//! touch, and which processing block implements it. The block is compiled to
//! WebAssembly and runs sandboxed inside the daemon, which serves inference
//! from a local model and streams results back to the caller.
//!
//! Three decisions shape the rest of the system, and each exists to avoid a
//! specific failure rather than to express a preference:
//!
//! **The host drives the guest.** A block is a state machine: it returns
//! *commands* (`Infer`, `Open`, `Slice`, `Done`) and the host executes them,
//! then steps the block again. The obvious alternative — let the guest call
//! into the host and block until it answers — cannot work, because a
//! single-threaded wasm guest offers no execution context for the host to call
//! back into, and inference must run on a different thread from the (`!Sync`)
//! wasm store. Inverting control also makes cancellation trivial: the host
//! simply stops stepping, so no guest cooperation is required.
//!
//! **Bulk data never enters guest memory.** A block receives a handle and a
//! length, then pulls bounded windows. Guest memory tracks the window size, not
//! the input size, so a block written against a README behaves identically
//! against a corpus — and the 4 GiB ceiling of 32-bit wasm binds on nothing we
//! actually do.
//!
//! **Capabilities are deny-by-default and checked twice.** A block reaches
//! nothing unless its spec grants it. The grant is verified when the spec is
//! compiled and again by the sandbox at runtime. The compile-time check is a
//! convenience that produces good error messages; the runtime check is the
//! security boundary.
//!
//! # This crate
//!
//! This is the facade. It ships the `cuttlefish` command-line client and
//! re-exports the workspace's pieces, so that one page documents the whole
//! system. The implementation lives in sibling crates.
//!
//! # Project conventions
//!
//! Documentation lives in the code rather than in a separate design document,
//! so that explanations sit beside what they explain and go stale loudly
//! instead of quietly. Comments state *why* code looks the way it does, never
//! what it does. See `AGENTS.md` in the repository for the full contract.
//!
//! [cuttlefish]: https://cuttlefishvm.github.io/

#![doc(html_logo_url = "https://cuttlefishvm.github.io/assets/images/logo.png")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

// No `#![cfg_attr(docsrs, feature(doc_auto_cfg))]`: that feature was removed in
// Rust 1.92 (merged into `doc_cfg`) and now hard-errors under `--cfg docsrs`.
// The crate has no cargo features to annotate anyway. If features are added
// later and their docs need "available on feature X" labels, reach for the
// then-current `doc_cfg` spelling and verify it against the pinned toolchain
// before committing — this exact line already broke CI once.
