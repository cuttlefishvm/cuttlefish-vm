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

use std::path::PathBuf;

/// The `Cargo.toml` every one-off fixture block shares — factored out so
/// [`block_with`] and [`block_with_source`] can't drift from each other.
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
        // Windows path in a normal TOML string makes `\a` an escape
        // sequence, so the manifest fails to parse and the fixture fails
        // to build — on one platform only, with an error that says
        // nothing about paths.
        sdk.display().to_string().replace('\\', "/")
    )
}

/// Compile a one-off block with the given signature and body, returning its
/// `.wasm` path.
///
/// Writing and compiling a real crate is slow but is the only way to test
/// what is actually claimed: that the declaration travels inside the module.
///
/// `support` is compiled fresh into each integration-test binary; binaries
/// that don't call this (`runner.rs`) would otherwise see it as dead code.
#[allow(dead_code)]
pub fn block_with(dir: &std::path::Path, name: &str, input: &str, output: &str) -> PathBuf {
    block_with_source(
        dir,
        name,
        &format!(
            r#"use cuttlefish_sdk::{{export_block, Block, Command, Event, Signature}};

#[derive(Default)]
struct B;

impl Block for B {{
    fn signature() -> Signature {{
        Signature {{
            input: "{input}".parse().unwrap(),
            output: "{output}".parse().unwrap(),
        }}
    }}
    fn start(&mut self, _input: serde_json::Value) -> Command {{
        Command::Done {{ result: serde_json::Value::Null }}
    }}
    fn step(&mut self, _event: Event) -> Command {{
        Command::Done {{ result: serde_json::Value::Null }}
    }}
}}

export_block!(B);
"#
        ),
    )
}

/// Compile a one-off block from a caller-supplied `src/lib.rs`, returning its
/// `.wasm` path.
///
/// This is [`block_with`]'s more general sibling: [`block_with`] can only
/// produce a block that immediately returns `Null`, which is enough for
/// typecheck-only fixtures but not for anything that needs to exercise real
/// `start`/`step` behavior (echoing input, looping, emitting a route,
/// panicking if invoked). Callers write the whole block body themselves —
/// still against `cuttlefish_sdk`, still exported with `export_block!`.
#[allow(dead_code)]
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
