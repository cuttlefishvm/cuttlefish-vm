//! Acceptance checks in isolation — what "done" means beyond a node's
//! declared type.
//!
//! The ladder that *reacts* to a failed check is tested separately in
//! `ladder.rs`; these tests are only about whether a check reaches the right
//! verdict, and about keeping the three judge outcomes distinguishable.

use cuttlefish_core::graph::AcceptCheck;
use cuttlefish_host::accept::{CompiledChecks, JudgeVerdict};
use cuttlefish_host::infer::{InferBackend, InferRequest, InferResult};
use std::sync::{Arc, Mutex};

/// A backend that returns one canned reply and records the prompt it saw.
struct CannedBackend {
    reply: String,
    seen: Arc<Mutex<String>>,
}

/// Build a backend returning `reply`, paired with a handle to whatever
/// prompt it last saw. Free function rather than `CannedBackend::new`: it
/// yields a trait object and a spy handle, not a `Self`.
fn canned(reply: &str) -> (Arc<dyn InferBackend>, Arc<Mutex<String>>) {
    {
        let seen = Arc::new(Mutex::new(String::new()));
        let backend: Arc<dyn InferBackend> = Arc::new(CannedBackend {
            reply: reply.to_string(),
            seen: seen.clone(),
        });
        (backend, seen)
    }
}

#[async_trait::async_trait]
impl InferBackend for CannedBackend {
    async fn infer(
        &self,
        req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        *self.seen.lock().unwrap() = req.prompt.to_string();
        Ok(InferResult {
            text: self.reply.clone(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    fn model_name(&self) -> String {
        "canned".to_string()
    }
}

fn schema_at(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("v.json");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn a_conforming_value_passes_and_a_broken_one_names_the_violation() {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_at(
        dir.path(),
        r#"{"type":"object","required":["verdict"],
            "properties":{"verdict":{"type":"string"}}}"#,
    );
    let checks = CompiledChecks::compile(&[AcceptCheck::Schema(schema)]).unwrap();

    assert!(checks
        .check_schemas(&serde_json::json!({"verdict": "pass"}))
        .is_ok());

    let err = checks
        .check_schemas(&serde_json::json!({"other": 1}))
        .expect_err("a value missing a required property must be rejected");
    assert!(
        err.contains("verdict"),
        "the violation must name the field, or the escalation is unactionable: {err}"
    );
}

#[test]
fn a_malformed_schema_fails_to_compile_rather_than_at_first_use() {
    // Caught at daemon startup, not mid-campaign.
    let dir = tempfile::tempdir().unwrap();
    let schema = schema_at(dir.path(), "not json at all");
    let err = CompiledChecks::compile(&[AcceptCheck::Schema(schema)])
        .expect_err("a malformed schema must fail at compile time");
    assert!(err.to_string().contains("v.json"), "{err}");
}

#[test]
fn a_missing_schema_file_fails_to_compile() {
    let err = CompiledChecks::compile(&[AcceptCheck::Schema("/no/such/schema.json".into())])
        .expect_err("a missing schema must fail at compile time");
    assert!(err.to_string().contains("schema.json"), "{err}");
}

#[tokio::test]
async fn a_judge_accepts_rejects_and_reports_an_unusable_verdict_distinctly() {
    let judge = AcceptCheck::Judge {
        model: None,
        prompt: "grade it".into(),
    };
    let checks = CompiledChecks::compile(std::slice::from_ref(&judge)).unwrap();
    let input = serde_json::json!({"chunk": "x"});
    let output = serde_json::json!({"finding": "y"});

    let (backend, _) = canned(r#"{"accept": true, "reason": "fine"}"#);
    let v = checks
        .run_judges(&input, &output, &backend, &Default::default())
        .await;
    assert!(matches!(v, JudgeVerdict::Accepted), "got {v:?}");

    // The reason must survive: it becomes the escalation text a human reads.
    let (backend, _) = canned(r#"{"accept": false, "reason": "no numbers cited"}"#);
    let v = checks
        .run_judges(&input, &output, &backend, &Default::default())
        .await;
    match v {
        JudgeVerdict::Rejected(reason) => assert_eq!(reason, "no numbers cited"),
        other => panic!("expected a rejection, got {other:?}"),
    }

    // NOT a rejection. "The judge never gave a verdict" and "the judge read
    // this and said no" demand different responses from whoever reads the
    // escalation; collapsing them wastes that person's afternoon.
    let (backend, _) = canned("I think it's probably fine?");
    let v = checks
        .run_judges(&input, &output, &backend, &Default::default())
        .await;
    assert!(matches!(v, JudgeVerdict::Unusable(_)), "got {v:?}");
}

#[tokio::test]
async fn a_judge_prompt_carries_both_the_input_and_the_output() {
    // "does this cite numbers *from the input*" is unanswerable otherwise.
    let judge = AcceptCheck::Judge {
        model: None,
        prompt: "grade it".into(),
    };
    let checks = CompiledChecks::compile(std::slice::from_ref(&judge)).unwrap();
    let (backend, seen) = canned(r#"{"accept": true, "reason": ""}"#);

    checks
        .run_judges(
            &serde_json::json!({"chunk": "INPUT-MARKER"}),
            &serde_json::json!({"finding": "OUTPUT-MARKER"}),
            &backend,
            &Default::default(),
        )
        .await;

    let prompt = seen.lock().unwrap().clone();
    assert!(
        prompt.contains("grade it"),
        "author's prompt missing: {prompt}"
    );
    assert!(prompt.contains("INPUT-MARKER"), "input missing: {prompt}");
    assert!(prompt.contains("OUTPUT-MARKER"), "output missing: {prompt}");
}

#[tokio::test]
async fn a_node_with_no_checks_accepts_everything() {
    let checks = CompiledChecks::compile(&[]).unwrap();
    assert!(checks.check_schemas(&serde_json::json!("anything")).is_ok());
    let (backend, _) = canned("never called");
    let v = checks
        .run_judges(
            &serde_json::Value::Null,
            &serde_json::json!("anything"),
            &backend,
            &Default::default(),
        )
        .await;
    assert!(matches!(v, JudgeVerdict::Accepted));
}
