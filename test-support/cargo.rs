//! Cargo subprocesses used by integration tests across the workspace.
//!
//! `cargo llvm-cov` instruments the parent test through `RUSTC_WRAPPER` and
//! its `CARGO_LLVM_COV*` and `__CARGO_LLVM_COV*` environment. A nested Cargo
//! process inherits that instrumentation unless it is removed explicitly.
//! Wasm has no profiler runtime, so an instrumented guest build fails with
//! a missing `profiler_builtins` crate. Native helper builds also need their
//! ordinary artifact layout rather than the coverage build's layout.
//!
//! Keep this constructor shared by the CLI, host and daemon tests so each
//! nested build gets the same environment isolation. Only the child command
//! is changed; the parent test keeps its instrumentation and coverage data.

/// Construct a nested Cargo command without the parent's coverage wrapper.
pub fn clean_cargo(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    for (key, _) in std::env::vars() {
        if key == "RUSTC_WRAPPER"
            || key.starts_with("CARGO_LLVM_COV")
            || key.starts_with("__CARGO_LLVM_COV")
        {
            cmd.env_remove(key);
        }
    }
    cmd
}
