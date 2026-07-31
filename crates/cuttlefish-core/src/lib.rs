//! Parsing and validation of `Cuttlefish.spec` files.
//!
//! Stub: the spec scanner lands here next.
//!
//! What goes in this crate is deliberately narrow — turning spec text into a
//! typed description of a job, and *rejecting* anything it does not fully
//! understand rather than silently accepting a partial parse. A spec that
//! half-parses would grant capabilities nobody wrote down.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
