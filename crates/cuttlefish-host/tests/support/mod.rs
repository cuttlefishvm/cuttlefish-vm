//! Shared support for tests that build wasm fixtures by shelling out to cargo.
//!
//! # Why nested cargo needs a clean environment
//!
//! `cargo llvm-cov` runs the test binary with `RUSTC_WRAPPER` and several
//! `CARGO_LLVM_COV*`/`__CARGO_LLVM_COV*` variables set, so that *its own*
//! `rustc` invocations get instrumented. Those variables are inherited by any
//! child process a test spawns — including, here, a fresh `cargo build` for a
//! wasm fixture.
//!
//! The wrapper does not expect to be invoked that way and the nested build
//! fails outright. The failure is invisible under a plain `cargo test`, which
//! sets none of this, and only appears under `cargo llvm-cov`. That mismatch is
//! exactly how it went unnoticed: CI's coverage job silently fell back to its
//! "no data" branch and reported an "unknown" badge instead of failing loudly,
//! and every other job runs plain `cargo test`.
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
