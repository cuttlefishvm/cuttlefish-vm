//! The wasmtime host: drives proc-blocks and enforces what they may reach.
//!
//! Stub: the runner, capability checks, and file-handle table land here next.
//!
//! This is the crate where the project's security boundary actually lives. The
//! compile-time capability check in [`cuttlefish_core`] produces good error
//! messages; the checks *here* are what a malicious or malfunctioning block
//! actually runs into, and they fail closed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
