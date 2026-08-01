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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod caps;
pub mod documents;
pub mod handles;
pub mod infer;
#[cfg(feature = "llamacpp")]
pub mod llamacpp;
pub mod ollama;
/// Rendering PDF pages out-of-process, so a renderer crash cannot take the
/// daemon with it.
#[cfg(feature = "pdf-render")]
pub mod render_worker;
pub mod runner;
