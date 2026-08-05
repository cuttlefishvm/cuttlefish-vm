//! `cuttlefish block new`, driven through the real binary.

use std::path::Path;
use std::process::Command;

const CUTTLEFISH: &str = env!("CARGO_BIN_EXE_cuttlefish");

fn run_block_new(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(CUTTLEFISH)
        .args(["block", "new"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("the cuttlefish binary under test must be spawnable")
}

#[test]
fn block_new_rust_scaffolds_a_working_crate() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run_block_new(
        tmp.path(),
        &[
            "my-echo",
            "--input",
            "{n: json}",
            "--output",
            "{n: json}",
            "--lang",
            "rust",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let crate_dir = tmp.path().join(".cuttlefish/blocks/my-echo");
    assert!(crate_dir.join("Cargo.toml").exists());
    assert!(crate_dir.join("src/lib.rs").exists());

    // Actually build it — proves the scaffold is real, working Rust, not
    // just plausible-looking text.
    let build = Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .args(["--target", "wasm32-unknown-unknown"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
}

#[test]
fn block_new_rhai_scaffolds_a_script_with_a_signature_header() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run_block_new(
        tmp.path(),
        &["my-script", "--input", "{n: json}", "--output", "{n: json}"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let script_path = tmp.path().join(".cuttlefish/blocks/my-script/block.rhai");
    assert!(script_path.exists());
    let contents = std::fs::read_to_string(&script_path).unwrap();
    assert!(
        contents.contains("//! signature: {n: json} -> {n: json}"),
        "{contents}"
    );
    // No Cargo.toml — the Rhai path needs no crate at all.
    assert!(!tmp
        .path()
        .join(".cuttlefish/blocks/my-script/Cargo.toml")
        .exists());
}

#[test]
fn block_new_refuses_to_overwrite_an_existing_block() {
    let tmp = tempfile::tempdir().unwrap();
    let args = ["dup", "--input", "json", "--output", "json"];
    let first = run_block_new(tmp.path(), &args);
    assert!(first.status.success());

    let second = run_block_new(tmp.path(), &args);
    assert!(!second.status.success());
}

#[test]
fn block_new_rejects_an_invalid_name() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run_block_new(tmp.path(), &["con", "--input", "json", "--output", "json"]);
    assert!(!output.status.success());
}

#[test]
fn block_new_rejects_an_unparseable_type_string() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run_block_new(
        tmp.path(),
        &[
            "ok-name",
            "--input",
            "not a real type(((",
            "--output",
            "json",
        ],
    );
    assert!(!output.status.success());
}
