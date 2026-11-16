//! Shared support for tests that build one-off wasm block fixtures by
//! shelling out to cargo.
//!
//! Mirrors `crates/cuttlefish-host/tests/support/mod.rs`. Proving that
//! `cuttlefishd`'s `--wasm` flag actually swaps which bytes the daemon runs —
//! rather than merely being accepted as an argument — needs two real,
//! distinguishable compiled blocks driven through the real wasm boundary; a
//! mock can't stand in for that.

use std::path::PathBuf;

#[path = "../../../../test-support/cargo.rs"]
pub mod cargo;

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

    let status = cargo::clean_cargo(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo should start");
    assert!(status.success(), "building the fixture block {name} failed");

    crate_dir
        .join("target/wasm32-unknown-unknown/debug")
        .join(format!("{}.wasm", name.replace('-', "_")))
}
