//! The wasmtime host: drives proc-blocks and enforces what they may reach.
//!
//! This crate is where the project's security boundary actually lives. The
//! compile-time capability check in `cuttlefish-core` exists to give spec
//! authors good error messages; the checks in [`caps`] are what a malicious or
//! malfunctioning block actually runs into, and they fail closed.
//!
//! Three pieces, in the order a job meets them:
//!
//! - [`caps`] — what a job may reach. Deny-by-default, and canonicalizing to
//!   defeat traversal and symlink escapes.
//! - [`handles`] — files held open on the guest's behalf, served as bounded
//!   windows so that bulk data never enters guest memory.
//! - [`runner`] — the reactor loop: the host drives the guest one command at a
//!   time, which is what makes cancellation free and every iteration
//!   observable.
//!
//! Inference reaches the runner only through [`infer::InferBackend`], so the
//! whole loop is testable with no model present. Which implementation a job gets
//! is decided by [`backend::Registry`], so adding a provider — an
//! OpenAI-compatible endpoint, an embedded llama.cpp — is additive rather than a
//! change to the runner, the parser, or the daemon. [`ollama`] is the first real
//! one.
//!
//! [`catalog`] is a local, content-addressed store mapping `name@version` to
//! a cataloged wasm block or bundle, so a pipeline can reference a block by
//! name instead of a filesystem path. Purely local filesystem operations —
//! no network. The daemon does consult it (resolving a spec's pipeline
//! entries at startup, via [`pipeline::resolve_and_load`]), but the catalog
//! itself has no daemon-specific logic: the same resolution runs identically
//! from `cuttlefish build`.
//!
//! [`bundle`] packages a [`pipeline::Checked`] pipeline into the `.cfbundle`
//! container `cuttlefish build` emits — the write side of what
//! `catalog`'s `read_bundle_signature` reads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod accept;
pub mod backend;
pub mod bundle;
pub mod caps;
pub mod catalog;
pub mod dag;
pub mod documents;
pub mod fetch;
pub mod handles;
pub mod hex;
pub mod images;
pub mod infer;
pub mod ledger;
#[cfg(feature = "llamacpp")]
pub mod llamacpp;
pub mod module_cache;
pub mod ollama;
pub mod pipeline;

/// The shared Rhai interpreter's compiled bytes, embedded at compile time.
///
/// This is a checked-in binary asset (`assets/rhai-interpreter.wasm`), not
/// built dynamically as part of an ordinary `cargo build --workspace` —
/// `include_bytes!` is resolved by rustc while compiling *this* crate, so
/// the file must already exist on disk before this crate compiles, and
/// Cargo has no built-in way to cross-compile a sibling workspace member to
/// `wasm32-unknown-unknown` first as part of building this one natively.
/// Regenerate it with `scripts/rebuild-rhai-interpreter.sh` whenever
/// `blocks/rhai-interpreter`'s source changes, and commit the result — CI
/// independently checks the asset hasn't drifted (see
/// `.github/workflows/ci.yml`).
pub fn embedded_rhai_interpreter_bytes() -> &'static [u8] {
    include_bytes!("../assets/rhai-interpreter.wasm")
}
/// Rendering PDF pages out-of-process, so a renderer crash cannot take the
/// daemon with it.
///
/// Compiled unconditionally, unlike the rendering it performs. A binary that
/// *might* be spawned as a worker has to recognise the worker argument even
/// when it cannot render, or it falls through to its own argument parsing
/// and answers a render request with usage text — which then surfaces as a
/// per-item job failure reading `Error: usage: cuttlefishd <spec> ...` and
/// says nothing about the real mismatch.
pub mod render_worker;
pub mod runner;
