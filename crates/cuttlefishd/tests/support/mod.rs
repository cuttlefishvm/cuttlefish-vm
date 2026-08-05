//! Shared support for tests that build one-off wasm block fixtures by
//! shelling out to cargo.
//!
//! Mirrors `crates/cuttlefish-host/tests/support/mod.rs`. Proving that
//! `cuttlefishd`'s `--wasm` flag actually swaps which bytes the daemon runs —
//! rather than merely being accepted as an argument — needs two real,
//! distinguishable compiled blocks driven through the real wasm boundary; a
//! mock can't stand in for that.

use std::path::PathBuf;

/// A `cargo` invocation with the coverage-only environment variables that
/// break a *nested* cargo build stripped out.
///
/// `cargo llvm-cov` sets `RUSTC_WRAPPER`/`CARGO_LLVM_COV*` for its own
/// instrumented build; a child `cargo build` inherits and chokes on them
/// unless they're removed first. Invisible under a plain `cargo test`, which
/// sets none of this — see the sibling copy of this comment in
/// `crates/cuttlefish-host/tests/support/mod.rs` for the fuller story.
fn clean_cargo(program: &str) -> std::process::Command {
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

/// The `Cargo.toml` every one-off fixture block shares.
fn manifest(name: &str) -> String {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let sdk = workspace.join("crates/cuttlefish-sdk");

    format!(
        r#"[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
cuttlefish-sdk = {{ path = '{}' }}
serde_json = "1"

[workspace]
"#,
        // A TOML *literal* string (single quotes) and forward slashes: a
        // Windows path in a normal TOML string makes `\a` an escape sequence,
        // which would fail to parse on that platform only.
        sdk.display().to_string().replace('\\', "/")
    )
}

/// Compile a one-off block from a caller-supplied `src/lib.rs`, returning its
/// `.wasm` path.
pub fn block_with_source(dir: &std::path::Path, name: &str, lib_rs: &str) -> PathBuf {
    let crate_dir = dir.join(name);
    std::fs::create_dir_all(crate_dir.join("src")).unwrap();

    std::fs::write(crate_dir.join("Cargo.toml"), manifest(name)).unwrap();
    std::fs::write(crate_dir.join("src/lib.rs"), lib_rs).unwrap();

    let status = clean_cargo(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo should start");
    assert!(status.success(), "building the fixture block {name} failed");

    crate_dir
        .join("target/wasm32-unknown-unknown/debug")
        .join(format!("{}.wasm", name.replace('-', "_")))
}
