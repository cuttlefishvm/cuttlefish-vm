//! End-to-end tests over a real wasm boundary.
//!
//! These build the example block and run it inside wasmtime rather than mocking
//! the guest, which is the point: the ABI, the descriptor layout, the capability
//! checks, and the reactor loop are all exercised together, the way they will
//! actually be used. A mock would happily agree with a host that reads the
//! descriptor wrong.

use cuttlefish_abi::{error_codes, JobStatus};
use cuttlefish_host::{
    caps::Capabilities,
    infer::StubBackend,
    runner::{run_job, JobEvent, JobSpec},
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

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
        let status = std::process::Command::new(env!("CARGO"))
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

fn spec(f: &Fixture, input: serde_json::Value) -> JobSpec {
    JobSpec {
        stages: vec![example_block()],
        input,
        caps: f.caps.clone(),
    }
}

#[tokio::test]
async fn runs_a_job_end_to_end() {
    let f = fixture("some document text");
    let (tx, mut rx) = mpsc::channel(64);

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
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
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": secret.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
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
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        cancel,
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

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "wrong_field": 1 })),
        tx,
        CancellationToken::new(),
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

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            stages: vec![b"definitely not wasm".to_vec()],
            input: serde_json::json!({ "path": f.doc.to_str().unwrap() }),
            caps: f.caps.clone(),
        },
        tx,
        CancellationToken::new(),
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

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": f.doc.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
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
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(TextOnlyBackend),
        spec(&f, serde_json::json!({ "path": png.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
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
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        spec(&f, serde_json::json!({ "path": png.to_str().unwrap() })),
        tx,
        CancellationToken::new(),
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
