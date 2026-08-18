//! End-to-end tests over a real wasm boundary.
//!
//! These build the example block and run it inside wasmtime rather than mocking
//! the guest, which is the point: the ABI, the descriptor layout, the capability
//! checks, and the reactor loop are all exercised together, the way they will
//! actually be used. A mock would happily agree with a host that reads the
//! descriptor wrong.

mod support;

use cuttlefish_abi::{error_codes, JobStatus};
use cuttlefish_core::graph::InputExpr;
use cuttlefish_host::{
    caps::Capabilities,
    catalog::ArtifactKind,
    dag::CheckedNode,
    infer::StubBackend,
    ledger::Ledger,
    runner::{run_job, JobEvent, JobSpec},
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// A fresh, real per-test ledger backed by its own tempdir — same pattern
/// `ledger.rs`'s own tests use. Returned together with the `TempDir` so the
/// caller keeps it alive for the ledger's lifetime (the ledger's sqlite file
/// lives inside it).
fn test_ledger() -> (tempfile::TempDir, Ledger) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "test-fingerprint").unwrap();
    (dir, ledger)
}

/// Build the example block and return its wasm bytes.
///
/// Built here rather than checked in, so the fixture cannot drift from the SDK.
/// Always debug: `cargo test --release` would otherwise look in a `release/`
/// directory this never populates.
///
/// Built exactly once per test binary. Tests run concurrently by default, and
/// letting each one shell out to cargo makes them contend on the target
/// directory lock — which fails a losing test with a build error that has
/// nothing to do with what it was checking. This is not a speed optimisation;
/// it is what stops the suite being flaky.
fn example_block() -> Vec<u8> {
    static WASM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

    WASM.get_or_init(|| {
        let status = crate::support::clean_cargo(env!("CARGO"))
            .args([
                "build",
                "-p",
                "cf-block-echo-summarize",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building the example block failed");

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

struct Fixture {
    _dir: tempfile::TempDir,
    doc: std::path::PathBuf,
    caps: Capabilities,
}

fn fixture(contents: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, contents).unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    Fixture {
        _dir: dir,
        doc,
        caps,
    }
}

/// Build a [`CheckedNode`] for a test, with every field beyond the two that
/// actually vary here defaulted the way a plain, unconditional, non-looping
/// node's would be — a permissive signature and no `input`/`repeat_until`.
fn node(name: &str, module_bytes: Vec<u8>) -> CheckedNode {
    CheckedNode {
        name: name.to_string(),
        kind: ArtifactKind::Block,
        resolved: None,
        module_bytes,
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            output: cuttlefish_abi::Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: None,
        over: None,
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

fn spec(f: &Fixture, input: serde_json::Value) -> JobSpec {
    JobSpec {
        nodes: vec![node("block", example_block())],
        exclusive_to: HashMap::new(),
        input,
        caps: f.caps.clone(),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    }
}

#[tokio::test]
async fn runs_a_job_end_to_end() {
    let f = fixture("some document text");
    let (tx, mut rx) = mpsc::channel(64);

    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["summary"], "a stub summary");
    assert_eq!(result["path"], f.doc.to_str().unwrap());
    assert!(envelope.usage.tokens_out > 0, "usage must be accounted");
    assert_eq!(envelope.usage.model, "stub");

    let mut tokens = Vec::new();
    while let Ok(JobEvent::Token(t)) = rx.try_recv() {
        tokens.push(t);
    }
    assert!(!tokens.is_empty(), "tokens must reach the event stream");
}

#[tokio::test]
async fn denies_a_read_outside_the_granted_capability() {
    let f = fixture("irrelevant");
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    std::fs::write(&secret, "proprietary").unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": secret.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed);
    assert_eq!(envelope.error.unwrap().code, error_codes::CAPABILITY_DENIED);
    assert!(
        envelope.result.is_none(),
        "a failed job must never carry a partial result"
    );
}

#[tokio::test]
async fn a_guest_stop_verdict_truncates_generation() {
    // The interleaving test. If the host awaited inference to completion before
    // consulting the guest, every token would already exist and this would read
    // the full reply.
    let f = fixture("text");
    let (tx, _rx) = mpsc::channel(64);

    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(
            &f,
            serde_json::json!({
                "path": f.doc.to_str().unwrap(),
                "stop_after_first": true
            }),
        ),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let summary = envelope.result.unwrap()["summary"]
        .as_str()
        .unwrap()
        .to_string();

    // The stub's full reply is three tokens. Asserting "fewer than three" rather
    // than an exact string keeps this honest about the one-token lag between the
    // guest's verdict and the backend observing it.
    assert!(
        envelope.usage.tokens_out < 3,
        "stop must cut generation short, got {} tokens ({summary:?})",
        envelope.usage.tokens_out
    );
    assert_ne!(summary, "a stub summary");
}

#[tokio::test]
async fn cancelling_before_the_job_starts_yields_cancelled() {
    let f = fixture("text");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        cancel,
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Cancelled);
    assert_eq!(envelope.error.unwrap().code, error_codes::CANCELLED);
}

#[tokio::test]
async fn malformed_input_fails_with_a_code_rather_than_trapping() {
    // The block returns Command::Fail instead of panicking, so the caller gets
    // an actionable code rather than an opaque wasm trap.
    let f = fixture("text");
    let (tx, _rx) = mpsc::channel(64);

    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "wrong_field": 1 })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed);
    assert_eq!(
        envelope.error.unwrap().code,
        error_codes::SCHEMA_VALIDATION_FAILED
    );
}

#[tokio::test]
async fn a_module_that_is_not_wasm_fails_as_a_trap() {
    let f = fixture("text");
    let (tx, _rx) = mpsc::channel(64);

    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            nodes: vec![node("block", b"definitely not wasm".to_vec())],
            exclusive_to: HashMap::new(),
            input: serde_json::json!({ "path": f.doc.to_str().unwrap() }),
            caps: f.caps.clone(),
            alternates: Default::default(),
            embedder: None,
            warehouse: None,
        },
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed);
    assert_eq!(envelope.error.unwrap().code, error_codes::WASM_TRAP);
}

#[tokio::test]
async fn a_multibyte_document_survives_the_slice_boundary() {
    // The block reads one window, so this mainly proves the host does not hand
    // the guest a split character — which would surface as mojibake in the
    // prompt rather than as an error.
    let f = fixture("héllo wörld ✓ 日本語");
    let (tx, _rx) = mpsc::channel(64);

    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
}

/// A backend that cannot see images, to prove the runner refuses rather than
/// dropping them.
struct TextOnlyBackend;

#[async_trait::async_trait]
impl cuttlefish_host::infer::InferBackend for TextOnlyBackend {
    async fn infer(
        &self,
        _req: cuttlefish_host::infer::InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<cuttlefish_host::infer::InferResult> {
        Ok(cuttlefish_host::infer::InferResult {
            text: "an answer about nothing".into(),
            tokens_in: 1,
            tokens_out: 1,
        })
    }

    fn model_name(&self) -> String {
        "text-only".into()
    }
    // supports_images() defaults to false — that default is what is under test.
}

#[tokio::test]
async fn images_sent_to_a_text_only_backend_fail_loudly() {
    // The failure this guards is subtle: without it the backend above returns
    // "an answer about nothing" with status completed, and the caller has no way
    // to tell that its image was discarded. A confident wrong answer is worse
    // than an error.
    let f = fixture("irrelevant");
    let png = f.doc.parent().unwrap().join("image.png");
    std::fs::write(&png, [0x89, b'P', b'N', b'G', 13, 10, 26, 10]).unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(TextOnlyBackend),
        spec(&f, serde_json::json!({ "path": png.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Failed,
        "a discarded image must not look like success"
    );
    let error = envelope.error.expect("a failed job carries an error");
    assert_eq!(error.code, error_codes::UNSUPPORTED);
    assert!(
        error.message.contains("vision-capable"),
        "the message should say what to do about it: {}",
        error.message
    );
    assert!(envelope.result.is_none());
}

#[tokio::test]
async fn images_reach_a_backend_that_accepts_them() {
    // The other half: the guard must not block the working path. The stub
    // reports what it received, so this asserts the image actually arrived
    // rather than merely that the job succeeded.
    let f = fixture("irrelevant");
    let png = f.doc.parent().unwrap().join("ok.png");
    std::fs::write(&png, [0x89, b'P', b'N', b'G', 13, 10, 26, 10]).unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": png.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let summary = envelope.result.unwrap()["summary"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        summary.contains("1 image(s)"),
        "the image must actually reach the backend, got: {summary:?}"
    );
}

#[test]
fn a_block_declares_its_signature_through_the_module() {
    // The declaration comes from the artifact that will actually run, so it
    // cannot describe a different version of the block than the one checked.
    let sig = cuttlefish_host::runner::read_signature(&Engine::default(), &example_block())
        .expect("the example block should report a signature");

    assert_eq!(sig.input.to_string(), "{path: text}");
    assert_eq!(sig.output.to_string(), "{path: text, summary: text}");
}

#[test]
fn a_module_without_a_signature_reports_the_permissive_default() {
    // Blocks ship independently of the host. One built before signatures
    // existed must still compose — unchecked, but working.
    let wat = r#"(module (memory (export "memory") 1))"#;
    let bytes = wat::parse_str(wat).expect("valid wat");

    let sig = cuttlefish_host::runner::read_signature(&Engine::default(), &bytes)
        .expect("a module without cf_signature is not an error");
    assert_eq!(sig.input, cuttlefish_abi::Ty::Json);
    assert_eq!(sig.output, cuttlefish_abi::Ty::Json);
}

/// A block that ignores its input entirely and always returns the same fixed
/// JSON value — used to give two entry nodes distinguishable outputs even
/// though both receive the same job input.
fn const_block_source(struct_name: &str, value_json: &str) -> String {
    format!(
        r#"use cuttlefish_sdk::{{export_block, Block, Command, Event}};

#[derive(Default)]
struct {struct_name};

impl Block for {struct_name} {{
    fn start(&mut self, _input: serde_json::Value) -> Command {{
        Command::Done {{ result: serde_json::json!({value_json}) }}
    }}
    fn step(&mut self, _event: Event) -> Command {{
        unreachable!("this block never issues a command that produces an event")
    }}
}}

export_block!({struct_name});
"#
    )
}

/// A block that returns its input unchanged — lets a test observe exactly
/// what composed input a fan-in node actually received.
fn echo_block_source() -> String {
    r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct Echo;

impl Block for Echo {
    fn start(&mut self, input: serde_json::Value) -> Command {
        Command::Done { result: input }
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!("echo never issues a command that produces an event")
    }
}

export_block!(Echo);
"#
    .to_string()
}

#[tokio::test]
async fn a_fan_in_node_receives_both_upstream_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    let emit_a = std::fs::read(support::block_with_source(
        dir.path(),
        "fanin_emit_a",
        &const_block_source("EmitA", r#"{ "a": "from-a" }"#),
    ))
    .unwrap();
    let emit_b = std::fs::read(support::block_with_source(
        dir.path(),
        "fanin_emit_b",
        &const_block_source("EmitB", r#"{ "b": "from-b" }"#),
    ))
    .unwrap();
    let echo = std::fs::read(support::block_with_source(
        dir.path(),
        "fanin_echo",
        &echo_block_source(),
    ))
    .unwrap();

    let mut merged = node("merged", echo);
    merged.input = Some(InputExpr::Record(
        [
            ("x".to_string(), InputExpr::FromNode("node_a".to_string())),
            ("y".to_string(), InputExpr::FromNode("node_b".to_string())),
        ]
        .into_iter()
        .collect(),
    ));

    let job = JobSpec {
        nodes: vec![node("node_a", emit_a), node("node_b", emit_b), merged],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["x"]["a"], "from-a",
        "the fan-in node's input should carry node_a's output under `x`: {result}"
    );
    assert_eq!(
        result["y"]["b"], "from-b",
        "the fan-in node's input should carry node_b's output under `y`: {result}"
    );
}

#[tokio::test]
async fn a_repeat_until_loop_stops_at_max_iterations_without_done() {
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    // Always reports "not-yet", carrying forward a counter purely so the
    // failure message (and a human) can see how many times it ran.
    let looping = std::fs::read(support::block_with_source(
        dir.path(),
        "loop_never_done",
        r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct NeverDone;

impl Block for NeverDone {
    fn start(&mut self, input: serde_json::Value) -> Command {
        let n = input.get("n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        Command::Done { result: serde_json::json!({ "done": "not-yet", "n": n }) }
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!("this block never issues a command that produces an event")
    }
}

export_block!(NeverDone);
"#,
    ))
    .unwrap();

    let mut looping_node = node("looper", looping);
    looping_node.repeat_until = Some("done".to_string());
    looping_node.max_iterations = Some(3);

    let job = JobSpec {
        nodes: vec![looping_node],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Failed,
        "a loop that never reaches done must fail loudly, not truncate silently"
    );
    assert_eq!(
        envelope.error.unwrap().code,
        error_codes::SCHEMA_VALIDATION_FAILED
    );
    assert!(envelope.result.is_none());
}

#[tokio::test]
async fn a_repeat_until_loop_stops_early_on_done() {
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    // Reports "done" on its very first invocation. Its own output carries an
    // incrementing counter, so if the runner kept looping past the first
    // "done" the final `n` would be greater than 1.
    let done_first = std::fs::read(support::block_with_source(
        dir.path(),
        "loop_done_first",
        r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct DoneFirst;

impl Block for DoneFirst {
    fn start(&mut self, input: serde_json::Value) -> Command {
        let n = input.get("n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        Command::Done { result: serde_json::json!({ "done": "done", "n": n }) }
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!("this block never issues a command that produces an event")
    }
}

export_block!(DoneFirst);
"#,
    ))
    .unwrap();

    let mut looping_node = node("looper", done_first);
    looping_node.repeat_until = Some("done".to_string());
    looping_node.max_iterations = Some(5); // much larger than the 1 call expected

    let job = JobSpec {
        nodes: vec![looping_node],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["n"], 1,
        "a block reporting done on its first call must run exactly once, got: {result}"
    );
}

#[tokio::test]
async fn a_node_exclusive_to_a_non_taken_branch_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    // Emits route "scan" — never "pdf".
    let router = std::fs::read(support::block_with_source(
        dir.path(),
        "branch_router",
        &const_block_source("Router", r#"{ "route": "scan" }"#),
    ))
    .unwrap();
    // Must never actually execute: it traps as soon as the host tries to
    // instantiate and call it, so the job would fail loudly if this ran.
    let must_not_run = std::fs::read(support::block_with_source(
        dir.path(),
        "branch_must_not_run",
        r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct MustNotRun;

impl Block for MustNotRun {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("this node is exclusive to a branch that was not taken and must not run");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(MustNotRun);
"#,
    ))
    .unwrap();

    // Exclusive to the branch that IS taken, with a distinctive output so
    // the job result proves *this* node ran rather than merely that the job
    // completed.
    let scan_only = std::fs::read(support::block_with_source(
        dir.path(),
        "branch_scan_only",
        &const_block_source("ScanOnly", r#"{ "ran": "scan_only" }"#),
    ))
    .unwrap();

    // Both of `router`'s declared labels get an entry, matching what
    // `dag::compute_branch_exclusivity` would seed from a real `branches`
    // decision that declares every label it can produce — a decision with
    // only one label declared would itself be the route-validation bug this
    // suite guards against.
    let mut exclusive_to = HashMap::new();
    exclusive_to.insert(
        "pdf_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "pdf".to_string(),
        },
    );
    exclusive_to.insert(
        "scan_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "scan".to_string(),
        },
    );

    let job = JobSpec {
        nodes: vec![
            node("router", router),
            node("pdf_only", must_not_run),
            node("scan_only", scan_only),
        ],
        exclusive_to,
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Completed,
        "the job must complete: the gated node must be skipped, not run (envelope: {envelope:?})"
    );
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["ran"], "scan_only",
        "the job result should be the node exclusive to the taken branch: {result}"
    );
}

#[tokio::test]
async fn an_unmatched_route_value_fails_the_job_loudly() {
    // The router's declared labels are "pdf" and "scan" (per exclusive_to,
    // below), but it emits "totally_wrong_value" — a typo'd or otherwise
    // unmatched route. Per the design spec's "Conditional dispatch" section,
    // this must fail the job loudly rather than silently skip every
    // branch-exclusive node and let an earlier node's output stand in as
    // the result.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    let router = std::fs::read(support::block_with_source(
        dir.path(),
        "unmatched_router",
        &const_block_source("Router", r#"{ "route": "totally_wrong_value" }"#),
    ))
    .unwrap();
    // Neither branch target may run: an unmatched route must fail before
    // either is reached.
    let must_not_run_src = r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct MustNotRun;

impl Block for MustNotRun {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("this node must not run when the route is unmatched");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(MustNotRun);
"#;
    let pdf_only = std::fs::read(support::block_with_source(
        dir.path(),
        "unmatched_pdf_only",
        must_not_run_src,
    ))
    .unwrap();
    let scan_only = std::fs::read(support::block_with_source(
        dir.path(),
        "unmatched_scan_only",
        must_not_run_src,
    ))
    .unwrap();

    let mut exclusive_to = HashMap::new();
    exclusive_to.insert(
        "pdf_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "pdf".to_string(),
        },
    );
    exclusive_to.insert(
        "scan_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "scan".to_string(),
        },
    );

    let job = JobSpec {
        nodes: vec![
            node("router", router),
            node("pdf_only", pdf_only),
            node("scan_only", scan_only),
        ],
        exclusive_to,
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Failed,
        "an unmatched route must fail the job, not silently skip every branch \
         and complete with a stale result (envelope: {envelope:?})"
    );
    assert_eq!(
        envelope.error.unwrap().code,
        error_codes::SCHEMA_VALIDATION_FAILED
    );
    assert!(
        envelope.result.is_none(),
        "a failed job must never carry a partial result"
    );
}

#[tokio::test]
async fn a_missing_route_field_on_a_decision_node_fails_the_job_loudly() {
    // The router is a genuine branches decision (per exclusive_to, below),
    // but its output carries no `route` field at all. Without the fix, no
    // entry is ever recorded in `route_taken` for this decision, so the
    // branch-skip check never fires and every branch-exclusive node runs
    // unconditionally — defeating conditional dispatch entirely. Assert the
    // job fails instead.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    // Returns a plain object with no `route` field.
    let router = std::fs::read(support::block_with_source(
        dir.path(),
        "routeless_router",
        &const_block_source("Router", r#"{ "something_else": "x" }"#),
    ))
    .unwrap();
    let must_not_run_src = r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct MustNotRun;

impl Block for MustNotRun {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("this node must not run when the decision node produced no route");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(MustNotRun);
"#;
    let pdf_only = std::fs::read(support::block_with_source(
        dir.path(),
        "routeless_pdf_only",
        must_not_run_src,
    ))
    .unwrap();
    let scan_only = std::fs::read(support::block_with_source(
        dir.path(),
        "routeless_scan_only",
        must_not_run_src,
    ))
    .unwrap();

    let mut exclusive_to = HashMap::new();
    exclusive_to.insert(
        "pdf_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "pdf".to_string(),
        },
    );
    exclusive_to.insert(
        "scan_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "scan".to_string(),
        },
    );

    let job = JobSpec {
        nodes: vec![
            node("router", router),
            node("pdf_only", pdf_only),
            node("scan_only", scan_only),
        ],
        exclusive_to,
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Failed,
        "a decision node with no route field must fail the job, not run every \
         branch-exclusive node unconditionally (envelope: {envelope:?})"
    );
    assert_eq!(
        envelope.error.unwrap().code,
        error_codes::SCHEMA_VALIDATION_FAILED
    );
    assert!(
        envelope.result.is_none(),
        "a failed job must never carry a partial result"
    );
}

#[tokio::test]
async fn resuming_skips_a_node_already_checkpointed_in_the_ledger() {
    // Simulates the resume path a later task's `/resume` endpoint will
    // drive: a ledger that already has a completed checkpoint for the first
    // node, handed to a fresh `run_job` call. The first node must not
    // re-run (it panics if it does), and its cached output must be exactly
    // what the second node sees.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    let must_not_run = std::fs::read(support::block_with_source(
        dir.path(),
        "resume_must_not_run",
        r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct MustNotRun;

impl Block for MustNotRun {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("this node already has a completed checkpoint and must not re-run");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(MustNotRun);
"#,
    ))
    .unwrap();
    let echo = std::fs::read(support::block_with_source(
        dir.path(),
        "resume_echo",
        &echo_block_source(),
    ))
    .unwrap();

    let mut second = node("second", echo);
    second.input = Some(InputExpr::FromNode("first".to_string()));

    let job = JobSpec {
        nodes: vec![node("first", must_not_run), second],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (_ledger_dir, ledger) = test_ledger();
    let cached_output = serde_json::json!({ "from": "checkpoint" });
    ledger.write_completed("first", &cached_output).unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Completed,
        "resuming past an already-checkpointed node must not fail the job: {envelope:?}"
    );
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result, cached_output,
        "the second node (an echo) must have received exactly the checkpointed output"
    );
}

#[tokio::test]
async fn resuming_rebuilds_route_taken_from_a_cached_decision_nodes_checkpoint() {
    // A decision node whose checkpoint is already `completed` (as if a prior
    // run picked a route and then the process died before reaching either
    // branch target) must still gate its branch-exclusive children
    // correctly on resume: the branch NOT taken must stay skipped, and the
    // branch that WAS taken must actually run — even though the decision
    // node itself is never re-executed to produce that route "live".
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    // Must never run: proves this test exercises the cached-route path, not
    // a live re-execution of the decision node.
    let router = std::fs::read(support::block_with_source(
        dir.path(),
        "resume_router",
        r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct Router;

impl Block for Router {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("the decision node already has a completed checkpoint and must not re-run");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(Router);
"#,
    ))
    .unwrap();
    let must_not_run_src = r#"use cuttlefish_sdk::{export_block, Block, Command, Event};

#[derive(Default)]
struct MustNotRun;

impl Block for MustNotRun {
    fn start(&mut self, _input: serde_json::Value) -> Command {
        panic!("this node is exclusive to a branch that was not taken and must not run");
    }
    fn step(&mut self, _event: Event) -> Command {
        unreachable!()
    }
}

export_block!(MustNotRun);
"#;
    let pdf_only = std::fs::read(support::block_with_source(
        dir.path(),
        "resume_pdf_only",
        must_not_run_src,
    ))
    .unwrap();
    let scan_only = std::fs::read(support::block_with_source(
        dir.path(),
        "resume_scan_only",
        &const_block_source("ScanOnly", r#"{ "ran": "scan_only" }"#),
    ))
    .unwrap();

    let mut exclusive_to = HashMap::new();
    exclusive_to.insert(
        "pdf_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "pdf".to_string(),
        },
    );
    exclusive_to.insert(
        "scan_only".to_string(),
        cuttlefish_host::dag::BranchExclusivity {
            decision: "router".to_string(),
            label: "scan".to_string(),
        },
    );

    let job = JobSpec {
        nodes: vec![
            node("router", router),
            node("pdf_only", pdf_only),
            node("scan_only", scan_only),
        ],
        exclusive_to,
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (_ledger_dir, ledger) = test_ledger();
    // The decision was already made and checkpointed before the (simulated)
    // restart — "scan" was chosen.
    ledger
        .write_completed("router", &serde_json::json!({ "route": "scan" }))
        .unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Completed,
        "resuming past a cached decision node must still gate its branches correctly: {envelope:?}"
    );
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["ran"], "scan_only",
        "the branch actually taken (per the cached route) must run: {result}"
    );
}

#[tokio::test]
async fn a_node_whose_output_violates_its_declared_signature_fails_loudly() {
    // Declares `{summary: text}` but always returns `{a: "from-a"}` — the
    // exact shape of mistake `cuttlefish block new`'s identity-stub template
    // makes trivially easy to write and never notice: nothing downstream
    // failed before this check existed, it just silently got `null` for
    // `summary`.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    let wrong_shape = std::fs::read(support::block_with_source(
        dir.path(),
        "wrong_shape",
        &const_block_source("WrongShape", r#"{ "a": "from-a" }"#),
    ))
    .unwrap();

    let mut mismatched = node("mismatched", wrong_shape);
    mismatched.signature = cuttlefish_abi::Signature {
        input: cuttlefish_abi::Ty::Json,
        output: cuttlefish_abi::Ty::Record(
            [("summary".to_string(), cuttlefish_abi::Ty::Text)]
                .into_iter()
                .collect(),
        ),
    };

    let job = JobSpec {
        nodes: vec![mismatched],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps,
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let (tx, _rx) = mpsc::channel(64);
    let (_ledger_dir, ledger) = test_ledger();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &cuttlefish_host::module_cache::ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let error = envelope.error.expect("a failed job carries an error");
    assert_eq!(error.code, error_codes::SCHEMA_VALIDATION_FAILED);
    assert!(
        error.message.contains("mismatched") && error.message.contains("summary"),
        "expected the mismatch to name the node and the missing field: {}",
        error.message
    );
}
