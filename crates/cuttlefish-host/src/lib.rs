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

pub mod backend;
pub mod bundle;
pub mod caps;
pub mod catalog;
pub mod dag;
pub mod documents;
pub mod handles;
pub mod infer;
pub mod ledger;
#[cfg(feature = "llamacpp")]
pub mod llamacpp;
pub mod ollama;
pub mod pipeline;

/// The shared Rhai interpreter's compiled bytes, embedded at compile time.
///
/// Stub for now — returns a tiny placeholder wasm module (a real, valid,
/// minimal module), not the real interpreter, so `pipeline::resolve_and_load`
/// has something to resolve `Script`-kind entries to before the real
/// interpreter exists. A later task replaces this function's body with a
/// real `include_bytes!` of a checked-in interpreter asset — the same item,
/// same signature, not a new one.
// TODO(later task): replace with the real embedded interpreter.
pub fn embedded_rhai_interpreter_bytes() -> &'static [u8] {
    // The smallest valid wasm module: magic bytes + version, no sections.
    const MINIMAL_VALID_WASM: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    MINIMAL_VALID_WASM
}
/// Rendering PDF pages out-of-process, so a renderer crash cannot take the
/// daemon with it.
#[cfg(feature = "pdf-render")]
pub mod render_worker;
pub mod runner;
