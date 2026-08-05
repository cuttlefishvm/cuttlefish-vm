//! End-to-end tests for the shared Rhai interpreter proc-block
//! (`blocks/rhai-interpreter`), driven through the real host exactly like
//! `runner.rs`'s example-block tests — building the real wasm and running
//! real jobs against it, not mocking the guest.

mod support;

use cuttlefish_abi::JobStatus;
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::CheckedNode;
use cuttlefish_host::infer::StubBackend;
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::runner::{run_job, JobSpec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// Build the real `cf-block-rhai-interpreter` crate to wasm32 and return its
/// bytes.
///
/// Built here rather than checked in, same reasoning `runner.rs`'s
/// `example_block()` gives: the fixture cannot drift from the crate that
/// actually ships. Built exactly once per test binary for the same
/// concurrency reason `example_block()` documents.
fn interpreter_wasm() -> Vec<u8> {
    static WASM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

    WASM.get_or_init(|| {
        let status = support::clean_cargo(env!("CARGO"))
            .args([
                "build",
                "-p",
                "cf-block-rhai-interpreter",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building the rhai interpreter failed");

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_rhai_interpreter.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

fn script_node(script_field_value: &str) -> CheckedNode {
    CheckedNode {
        name: "rhai".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            output: cuttlefish_abi::Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: Some(script_field_value.to_string()),
    }
}

/// Wraps `real_input` in the `{"__cuttlefish_script": ..., "input": ...}`
/// shape `run_stage` constructs for a Script-kind node — this test drives
/// `run_job` directly (below `run_stage`'s own injection point, which isn't
/// wired until a later task), so it has to build that shape itself for now.
fn wrapped_input(script: &str, real_input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "__cuttlefish_script": script,
        "input": real_input,
    })
}

#[tokio::test]
async fn a_rhai_scripted_block_computes_pure_output_through_the_real_host() {
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node("#{ doubled: input.n * 2 }")],
        exclusive_to: HashMap::new(),
        input: wrapped_input("#{ doubled: input.n * 2 }", serde_json::json!({ "n": 21 })),
        caps: Capabilities::new(Vec::new()),
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["doubled"], 42);
}

#[tokio::test]
async fn a_rhai_script_can_round_trip_an_infer_call_through_the_real_host() {
    let script = "#{ summary: infer(\"summarize: \" + input.text, 32) }";
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: wrapped_input(script, serde_json::json!({ "text": "some document text" })),
        caps: Capabilities::new(Vec::new()),
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["summary"], "a stub summary");
}
