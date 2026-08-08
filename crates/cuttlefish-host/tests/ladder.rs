//! The `on_fail` recovery ladder, end to end through the real host.
//!
//! Every assertion here is about *how many times* and *against which
//! backend* work happened, not merely about the final answer. A ladder that
//! never retried, or that rerouted to the same model, would produce an
//! identical final value in most of these cases — so counting is the only
//! thing that actually pins the behaviour down.

mod support;

use cuttlefish_abi::JobStatus;
use cuttlefish_core::graph::{AcceptCheck, Rung};
use cuttlefish_core::spec::ModelRef;
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::CheckedNode;
use cuttlefish_host::infer::{InferBackend, InferRequest, InferResult};
use cuttlefish_host::ledger::Ledger;
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::runner::{run_job, Alternates, JobSpec};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

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
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read(
            root.join("target/wasm32-unknown-unknown/debug/cf_block_rhai_interpreter.wasm"),
        )
        .unwrap()
    })
    .clone()
}

/// A backend that counts calls and tags its reply with its own name, so a
/// test can tell *which* backend answered.
struct CountingBackend {
    name: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl InferBackend for CountingBackend {
    async fn infer(
        &self,
        _req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(InferResult {
            text: self.name.clone(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }
    fn model_name(&self) -> String {
        self.name.clone()
    }
}

fn counting(name: &str) -> (Arc<dyn InferBackend>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let backend: Arc<dyn InferBackend> = Arc::new(CountingBackend {
        name: name.to_string(),
        calls: calls.clone(),
    });
    (backend, calls)
}

/// A node whose script echoes the model's reply, with `accept`/`on_fail`.
fn node(accept: Vec<AcceptCheck>, on_fail: Vec<Rung>) -> CheckedNode {
    CheckedNode {
        name: "work".to_string(),
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
        script: Some(r#"#{ from: infer("do the work", 32) }"#.to_string()),
        over: None,
        item_output: None,
        accept,
        on_fail,
    }
}

/// A schema demanding `from == "big"`, so only the strong model passes.
fn only_big_passes(dir: &Path) -> AcceptCheck {
    let path = dir.join("v.json");
    std::fs::write(
        &path,
        r#"{"type":"object","properties":{"from":{"const":"big"}},"required":["from"]}"#,
    )
    .unwrap();
    AcceptCheck::Schema(path)
}

async fn run(
    node: CheckedNode,
    backend: Arc<dyn InferBackend>,
    alternates: Alternates,
    ledger: &Ledger,
    dir: &Path,
) -> cuttlefish_abi::Envelope {
    let (tx, _rx) = mpsc::channel(1024);
    let job = JobSpec {
        nodes: vec![node],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.to_path_buf()]),
        alternates,
    };
    run_job(
        Arc::new(Engine::default()),
        backend,
        job,
        tx,
        CancellationToken::new(),
        ledger,
        &ModuleCache::new(),
    )
    .await
}

fn ledger_in(dir: &Path) -> Ledger {
    Ledger::open(&dir.join("ledger.sqlite"), "fp").unwrap()
}

#[tokio::test]
async fn a_node_with_no_ladder_attempts_once_and_fails() {
    // The unchanged default. Everything below is measured against this.
    let dir = tempfile::tempdir().unwrap();
    let (backend, calls) = counting("small");
    let envelope = run(
        node(vec![only_big_passes(dir.path())], vec![]),
        backend,
        Alternates::new(),
        &ledger_in(dir.path()),
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no ladder means one attempt"
    );
}

#[tokio::test]
async fn retry_makes_further_attempts_against_the_same_backend() {
    // `retry 2` = up to two *further* attempts, so three calls in total when
    // every one of them is rejected.
    let dir = tempfile::tempdir().unwrap();
    let (backend, calls) = counting("small");
    let envelope = run(
        node(vec![only_big_passes(dir.path())], vec![Rung::Retry(2)]),
        backend,
        Alternates::new(),
        &ledger_in(dir.path()),
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn reroute_engages_only_after_retries_are_spent_and_uses_the_other_backend() {
    // The load-bearing test for the alternates map: an implementation that
    // resolved every model to the same backend would pass every other test
    // in this file, so this asserts on *which* backend answered.
    let dir = tempfile::tempdir().unwrap();
    let (small, small_calls) = counting("small");
    let (big, big_calls) = counting("big");
    let big_model = ModelRef::new("stub", "big");
    let mut alternates = Alternates::new();
    alternates.insert(big_model.clone(), big);

    let envelope = run(
        node(
            vec![only_big_passes(dir.path())],
            vec![Rung::Retry(1), Rung::Reroute(big_model)],
        ),
        small,
        alternates,
        &ledger_in(dir.path()),
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    assert_eq!(
        envelope.result.unwrap()["from"],
        "big",
        "the accepted value must come from the rerouted model"
    );
    assert_eq!(
        small_calls.load(Ordering::SeqCst),
        2,
        "first attempt plus one retry, both on the original backend"
    );
    assert_eq!(big_calls.load(Ordering::SeqCst), 1, "then one reroute");
}

#[tokio::test]
async fn escalate_is_terminal_and_records_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger_in(dir.path());
    let (backend, calls) = counting("small");

    let envelope = run(
        node(
            vec![only_big_passes(dir.path())],
            vec![Rung::Retry(1), Rung::Escalate],
        ),
        backend,
        Alternates::new(),
        &ledger,
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "escalate consumes no attempt"
    );

    let escalations = ledger.escalations().unwrap();
    assert_eq!(escalations.len(), 1, "the give-up must be recorded");
    assert_eq!(escalations[0].node, "work");
    assert!(
        escalations[0].reason.contains("from"),
        "the reason must carry the failing check, or it's unactionable: {}",
        escalations[0].reason
    );
}

/// A backend that answers "small" once and "big" forever after — one
/// rejected attempt followed by an accepted one.
struct FlipsAfterFirst {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl InferBackend for FlipsAfterFirst {
    async fn infer(
        &self,
        _req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(InferResult {
            text: if n == 0 { "small" } else { "big" }.to_string(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }
    fn model_name(&self) -> String {
        "flips".to_string()
    }
}

#[tokio::test]
async fn a_rejected_attempt_leaves_no_checkpoint_behind() {
    // The durability rule: only a *concluded* outcome gets a ledger row. A
    // rejected attempt that is about to be retried is not a conclusion, and
    // recording it would make a transient rejection permanent across a
    // resume — the node would come back with the bad value already cached.
    let dir = tempfile::tempdir().unwrap();
    let ledger = ledger_in(dir.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let backend: Arc<dyn InferBackend> = Arc::new(FlipsAfterFirst {
        calls: calls.clone(),
    });

    let envelope = run(
        node(vec![only_big_passes(dir.path())], vec![Rung::Retry(1)]),
        backend,
        Alternates::new(),
        &ledger,
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one rejection, one retry");

    // Exactly one row, holding the *accepted* value — not the rejected one,
    // and not both.
    let checkpoint = ledger.get_completed("work").unwrap();
    assert_eq!(
        checkpoint,
        Some(serde_json::json!({"from": "big"})),
        "the checkpoint must hold what was accepted"
    );
    assert!(
        ledger.escalations().unwrap().is_empty(),
        "a ladder that succeeded must escalate nothing"
    );
}

#[tokio::test]
async fn an_accepted_first_attempt_climbs_no_rungs() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, calls) = counting("big");
    let envelope = run(
        node(
            vec![only_big_passes(dir.path())],
            vec![Rung::Retry(5), Rung::Escalate],
        ),
        backend,
        Alternates::new(),
        &ledger_in(dir.path()),
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "success must not retry");
}
