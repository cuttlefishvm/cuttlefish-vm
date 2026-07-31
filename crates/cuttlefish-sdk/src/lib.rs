//! Guest-side library for writing cuttlefish proc-blocks.
//!
//! Stub: the `Block` trait and the wasm export macro land here next.
//!
//! A block is a state machine that the host drives — it returns commands and is
//! stepped, rather than calling the host and blocking. This crate exists to hide
//! that inversion behind an ordinary-looking Rust trait, so block authors do not
//! hand-write state transitions. See [`cuttlefish_abi`] for why control is
//! inverted in the first place.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
