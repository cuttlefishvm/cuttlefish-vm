//! Tests for pipeline typechecking.
//!
//! The mismatch these catch is the one that actually happens when blocks are
//! composed, and its untyped failure mode is bad: a block handed the wrong shape
//! does not fail at the seam, it fails somewhere inside itself with a confusing
//! error about a missing field — or produces a plausible answer computed from
//! nothing.
//!
//! Blocks are compiled here from source rather than mocked, because the whole
//! premise is that a signature comes from the artifact that will run. A mocked
//! signature would test the checker against a fiction.

use cuttlefish_host::pipeline::{check, PipelineError};
use std::path::PathBuf;
use wasmtime::Engine;

/// Compile a one-off block with the given signature and body, returning its
/// `.wasm` path.
///
/// Writing and compiling a real crate is slow but is the only way to test what
/// is actually claimed: that the declaration travels inside the module.
fn block_with(dir: &std::path::Path, name: &str, input: &str, output: &str) -> PathBuf {
    let crate_dir = dir.join(name);
    std::fs::create_dir_all(crate_dir.join("src")).unwrap();

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let sdk = workspace.join("crates/cuttlefish-sdk");

    std::fs::write(
        crate_dir.join("Cargo.toml"),
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
            // sequence, so the manifest fails to parse and the fixture fails to
            // build — on one platform only, with an error that says nothing
            // about paths.
            sdk.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    std::fs::write(
        crate_dir.join("src/lib.rs"),
        format!(
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
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo should start");
    assert!(status.success(), "building the fixture block {name} failed");

    crate_dir
        .join("target/wasm32-unknown-unknown/debug")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

#[test]
fn a_pipeline_whose_seams_line_up_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let first = block_with(dir.path(), "seam_ok_a", "{path: text}", "{chunks: [text]}");
    let second = block_with(
        dir.path(),
        "seam_ok_b",
        "{chunks: [text]}",
        "{summary: text}",
    );

    let checked = check(&Engine::default(), &[first, second]).expect("the seams line up");

    assert_eq!(checked.stages().len(), 2);
    assert_eq!(checked.input().to_string(), "{path: text}");
    assert_eq!(checked.output().to_string(), "{summary: text}");
}

#[test]
fn a_mismatched_seam_is_rejected_naming_both_blocks_and_both_types() {
    // The error has to say enough to act on: which two blocks, and what each
    // side wanted. "type error" would send someone reading four files.
    let dir = tempfile::tempdir().unwrap();
    let producer = block_with(dir.path(), "seam_bad_a", "{path: text}", "{summary: text}");
    let consumer = block_with(dir.path(), "seam_bad_b", "{chunks: [text]}", "{out: text}");

    let err = check(&Engine::default(), &[producer, consumer])
        .err()
        .expect("a mismatched seam must be rejected");

    assert!(matches!(err, PipelineError::SeamMismatch { .. }), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("seam_bad_a"), "names the producer: {msg}");
    assert!(msg.contains("seam_bad_b"), "names the consumer: {msg}");
    assert!(
        msg.contains("{summary: text}"),
        "shows what was produced: {msg}"
    );
    assert!(
        msg.contains("{chunks: [text]}"),
        "shows what was expected: {msg}"
    );
}

#[test]
fn a_producer_may_add_fields_its_consumer_does_not_need() {
    // Extra output must not break a downstream block; otherwise every block
    // gaining a field breaks every pipeline it is in.
    let dir = tempfile::tempdir().unwrap();
    let wide = block_with(dir.path(), "wide_a", "{path: text}", "{a: text, b: text}");
    let narrow = block_with(dir.path(), "narrow_b", "{a: text}", "{out: text}");

    assert!(check(&Engine::default(), &[wide, narrow]).is_ok());
}

#[test]
fn a_json_seam_accepts_anything() {
    // `json` is the top type and the escape hatch. It is also, deliberately, the
    // default — an unannotated block composes, just without its seam checked.
    let dir = tempfile::tempdir().unwrap();
    let typed = block_with(dir.path(), "json_a", "{path: text}", "{a: text}");
    let loose = block_with(dir.path(), "json_b", "json", "{out: text}");

    assert!(check(&Engine::default(), &[typed, loose]).is_ok());
}

#[test]
fn a_specific_input_does_not_accept_json() {
    // The other direction must fail, or `json` anywhere would silently disable
    // checking for everything downstream of it.
    let dir = tempfile::tempdir().unwrap();
    let loose = block_with(dir.path(), "rev_a", "{path: text}", "json");
    let typed = block_with(dir.path(), "rev_b", "{needed: text}", "{out: text}");

    assert!(check(&Engine::default(), &[loose, typed]).is_err());
}

#[test]
fn a_single_block_pipeline_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let only = block_with(dir.path(), "single_a", "{path: text}", "{summary: text}");

    let checked = check(&Engine::default(), &[only]).expect("one block is a valid pipeline");
    assert_eq!(checked.stages().len(), 1);
    // With one stage the pipeline's type is that stage's type.
    assert_eq!(checked.input().to_string(), "{path: text}");
    assert_eq!(checked.output().to_string(), "{summary: text}");
}

#[test]
fn an_empty_pipeline_is_rejected() {
    let err = check(&Engine::default(), &[])
        .err()
        .expect("nothing to run");
    assert!(matches!(err, PipelineError::Empty));
}

#[test]
fn a_missing_block_file_names_the_path() {
    let err = check(&Engine::default(), &[PathBuf::from("/no/such/block.wasm")])
        .err()
        .expect("a missing block cannot be checked");
    assert!(err.to_string().contains("/no/such/block.wasm"), "{err}");
}

// -- execution --------------------------------------------------------------

/// Compile a block that echoes its input with a field added, so a chain's
/// stages can be told apart in the result.
fn tagging_block(dir: &std::path::Path, name: &str, tag: &str) -> PathBuf {
    let crate_dir = dir.join(name);
    std::fs::create_dir_all(crate_dir.join("src")).unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/cuttlefish-sdk");

    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n\
             cuttlefish-sdk = {{ path = '{}' }}\nserde_json = \"1\"\n\n[workspace]\n",
            sdk.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    std::fs::write(
        crate_dir.join("src/lib.rs"),
        format!(
            r#"use cuttlefish_sdk::{{export_block, Block, Command, Event}};

#[derive(Default)]
struct B;

impl Block for B {{
    fn start(&mut self, input: serde_json::Value) -> Command {{
        let mut seen: Vec<String> = input
            .get("seen")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        seen.push("{tag}".to_string());
        Command::Done {{ result: serde_json::json!({{ "seen": seen }}) }}
    }}
    fn step(&mut self, _event: Event) -> Command {{
        Command::Fail {{ code: "unexpected".into(), message: "no commands issued".into() }}
    }}
}}

export_block!(B);
"#
        ),
    )
    .unwrap();

    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo should start");
    assert!(status.success(), "building {name} failed");

    crate_dir
        .join("target/wasm32-unknown-unknown/debug")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

#[tokio::test]
async fn a_pipeline_threads_each_result_into_the_next_block() {
    use cuttlefish_host::{
        caps::Capabilities,
        infer::StubBackend,
        runner::{run_job, JobSpec},
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let first = std::fs::read(tagging_block(dir.path(), "chain_one", "first")).unwrap();
    let second = std::fs::read(tagging_block(dir.path(), "chain_two", "second")).unwrap();
    let third = std::fs::read(tagging_block(dir.path(), "chain_three", "third")).unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            stages: vec![first, second, third],
            input: serde_json::json!({}),
            caps: Capabilities::default(),
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    assert_eq!(envelope.status, cuttlefish_abi::JobStatus::Completed);
    // Order is the whole meaning of a pipeline: each block saw the previous
    // one's output, and they ran in the order written.
    assert_eq!(
        envelope.result.unwrap()["seen"],
        serde_json::json!(["first", "second", "third"])
    );
}

#[tokio::test]
async fn a_failing_stage_ends_the_job_and_names_the_stage() {
    use cuttlefish_host::{
        caps::Capabilities,
        infer::StubBackend,
        runner::{run_job, JobSpec},
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let good = std::fs::read(tagging_block(dir.path(), "fail_one", "first")).unwrap();

    // A later stage that is not wasm at all: it must fail, and the error must
    // say *which* block, or a long pipeline turns debugging into a hunt.
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            stages: vec![good, b"not wasm".to_vec()],
            input: serde_json::json!({}),
            caps: Capabilities::default(),
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    assert_eq!(envelope.status, cuttlefish_abi::JobStatus::Failed);
    let message = envelope.error.unwrap().message;
    assert!(
        message.contains("block 2"),
        "the failing stage must be named: {message}"
    );
    assert!(envelope.result.is_none());
}
