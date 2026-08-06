//! `cuttlefish validate-json`, driven through the real binary.

use std::io::Write;
use std::process::{Command, Stdio};

const CUTTLEFISH: &str = env!("CARGO_BIN_EXE_cuttlefish");

fn schema_file(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join("schema.json");
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn a_conforming_value_passed_via_input_exits_zero_and_silent() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_file(
        &dir,
        r#"{"type": "object", "required": ["verdict"], "properties": {"verdict": {"type": "string", "enum": ["pass", "fail"]}}}"#,
    );

    let output = Command::new(CUTTLEFISH)
        .args(["validate-json"])
        .arg(&schema)
        .args(["--input", r#"{"verdict": "pass"}"#])
        .output()
        .expect("the cuttlefish binary under test must be spawnable");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "expected silent success");
}

#[test]
fn a_nonconforming_value_exits_nonzero_and_names_every_violation() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_file(
        &dir,
        r#"{"type": "object", "required": ["verdict", "score"], "properties": {"verdict": {"type": "string", "enum": ["pass", "fail"]}, "score": {"type": "number"}}}"#,
    );

    let output = Command::new(CUTTLEFISH)
        .args(["validate-json"])
        .arg(&schema)
        .args(["--input", r#"{"verdict": "maybe"}"#])
        .output()
        .expect("the cuttlefish binary under test must be spawnable");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("score"), "{stderr}");
    assert!(stderr.contains("maybe"), "{stderr}");
}

#[test]
fn the_value_can_be_piped_in_over_stdin_instead_of_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_file(&dir, r#"{"type": "object", "required": ["n"]}"#);

    let mut child = Command::new(CUTTLEFISH)
        .args(["validate-json"])
        .arg(&schema)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the cuttlefish binary under test must be spawnable");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"n": 1}"#)
        .unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_invalid_schema_file_itself_is_reported_clearly_not_confused_with_a_value_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_file(&dir, "not json at all");

    let output = Command::new(CUTTLEFISH)
        .args(["validate-json"])
        .arg(&schema)
        .args(["--input", "{}"])
        .output()
        .expect("the cuttlefish binary under test must be spawnable");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not valid JSON"), "{stderr}");
}
