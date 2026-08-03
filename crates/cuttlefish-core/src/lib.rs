//! Parsing and validation of `Cuttlefish.spec` files.
//!
//! A spec describes one delegable job: which model runs it, what the job may
//! reach, and which proc-block implements it. See [`spec`] for the format this
//! build reads and, more importantly, for what it refuses — a spec grants
//! capabilities, so anything not fully understood is an error rather than
//! something to skip past.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod graph;
pub mod lex;
pub mod spec;
