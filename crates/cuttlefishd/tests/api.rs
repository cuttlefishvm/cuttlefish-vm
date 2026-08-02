//! End-to-end tests against a real daemon on a real unix socket.
//!
//! Nothing here is mocked: an actual `UnixListener`, an actual HTTP client, and
//! an actual wasm guest. That matters most for the SSE test, which only passes
//! because of the replay log — a broadcast-only implementation fails it every
//! time, and would have shipped looking correct.
//!
//! Unix-only: the daemon serves over a unix domain socket, so there is nothing
//! to exercise on Windows. The portable crates are still tested there.

#![cfg(unix)]

use cuttlefish_core::spec::{DataPolicy, ModelRef, Spec};
use cuttlefishd::{api, serve, state::JobStore};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// Build the example block once per test binary.
///
/// Shared via `OnceLock` because tests run concurrently and letting each shell
/// out to cargo makes them contend on the target-directory lock, failing a
/// loser with a build error unrelated to what it was checking.
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

struct Harness {
    client: reqwest::Client,
    _dir: tempfile::TempDir,
    doc: std::path::PathBuf,
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, "some document text").unwrap();
    let sock = dir.path().join("cf.sock");

    let spec = Spec {
        name: "summarize_docs".into(),
        description: "Use when a local file needs summarizing.".into(),
        model: ModelRef::new("stub", ""),
        data_policy: DataPolicy::LocalOnly,
        read_roots: vec![dir.path().to_path_buf()],
        pipeline: vec!["../blocks/echo-summarize".into()],
    };

    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(cuttlefish_host::infer::StubBackend::default()),
        jobs: JobStore::default(),
        spec: Arc::new(spec),
        stages: Arc::new(vec![example_block()]),
    };

    let sock_for_server = sock.clone();
    tokio::spawn(async move {
        let _ = serve::serve_unix(api::router(state), &sock_for_server).await;
    });

    // Wait for the socket to appear rather than sleeping a fixed duration,
    // which would be either flaky or slow depending on the machine.
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(sock.exists(), "daemon never bound its socket");

    Harness {
        // `as_path()`, not `&sock`: the sealed UnixSocketProvider trait covers
        // `&Path` and `PathBuf` but not `&PathBuf`.
        client: reqwest::Client::builder()
            .unix_socket(sock.as_path())
            .build()
            .unwrap(),
        _dir: dir,
        doc,
    }
}

impl Harness {
    async fn submit(&self, input: serde_json::Value) -> reqwest::Response {
        self.client
            .post("http://localhost/jobs")
            .json(&serde_json::json!({ "spec": "summarize_docs", "input": input }))
            .send()
            .await
            .unwrap()
    }

    async fn submit_ok(&self, input: serde_json::Value) -> String {
        let resp = self.submit(input).await;
        assert_eq!(resp.status(), 202);
        resp.json::<serde_json::Value>().await.unwrap()["job_id"]
            .as_str()
            .expect("submission must return a job_id")
            .to_string()
    }

    /// Poll until the job reaches a terminal state.
    async fn await_terminal(&self, id: &str) -> serde_json::Value {
        for _ in 0..500 {
            let body: serde_json::Value = self
                .client
                .get(format!("http://localhost/jobs/{id}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();

            match body["status"].as_str().unwrap_or("running") {
                "completed" | "failed" | "cancelled" => return body,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        panic!("job {id} never reached a terminal state");
    }
}

#[tokio::test]
async fn submits_and_completes_a_job() {
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;

    let body = h.await_terminal(&id).await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["envelope"]["result"]["summary"], "a stub summary");
    assert_eq!(body["envelope"]["usage"]["model"], "stub");
}

#[tokio::test]
async fn a_result_survives_never_watching_the_stream() {
    // The durability guarantee: a client that never opens /events still gets its
    // result. This is why envelopes are retained rather than only streamed.
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;

    let body = h.await_terminal(&id).await;
    assert!(body["envelope"]["result"].is_object());
}

#[tokio::test]
async fn streams_tokens_and_a_result_over_sse() {
    // Attaching is a second round trip, by which time this stub job has usually
    // finished — so this passes only because the store replays its event log to
    // late subscribers. A broadcast-only implementation fails here every run.
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;

    // Make sure the job is genuinely done before subscribing, so the test is
    // exercising replay rather than getting lucky with timing.
    h.await_terminal(&id).await;

    let resp = h
        .client
        .get(format!("http://localhost/jobs/{id}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The stream stays open (the store holds a live sender), so read chunks
    // until both markers appear rather than awaiting the whole body.
    let body = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = resp.bytes_stream();
        let mut seen = String::new();
        while let Some(chunk) = stream.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if seen.contains(r#""type":"token""#) && seen.contains(r#""type":"result""#) {
                break;
            }
        }
        seen
    })
    .await
    .expect("timed out waiting for token and result events");

    assert!(
        body.contains(r#""type":"token""#),
        "no token events: {body}"
    );
    assert!(
        body.contains(r#""type":"result""#),
        "no result event: {body}"
    );
}

#[tokio::test]
async fn a_capability_violation_is_reported_as_a_failed_job() {
    let h = start().await;
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    std::fs::write(&secret, "proprietary").unwrap();

    let id = h
        .submit_ok(serde_json::json!({ "path": secret.to_str().unwrap() }))
        .await;

    let body = h.await_terminal(&id).await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["envelope"]["error"]["code"], "capability_denied");
    assert!(
        body["envelope"]["result"].is_null(),
        "a failed job must not carry a result"
    );
}

#[tokio::test]
async fn an_unknown_job_is_not_found() {
    let h = start().await;
    let resp = h
        .client
        .get("http://localhost/jobs/does-not-exist")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn an_unknown_spec_is_rejected() {
    let h = start().await;
    let resp = h
        .client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({ "spec": "nope", "input": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn cancelling_an_unknown_job_is_not_found() {
    let h = start().await;
    let resp = h
        .client
        .delete("http://localhost/jobs/does-not-exist")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn cancel_is_accepted_and_the_job_still_reaches_a_terminal_state() {
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;

    let resp = h
        .client
        .delete(format!("http://localhost/jobs/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // Deliberately not asserting *which* terminal state: the job may well have
    // completed before the cancel landed. Pinning that would make this a test of
    // scheduling luck rather than of the endpoint.
    let body = h.await_terminal(&id).await;
    let status = body["status"].as_str().unwrap();
    assert!(
        matches!(status, "cancelled" | "completed"),
        "unexpected terminal status {status}"
    );
}

#[tokio::test]
async fn specs_are_discoverable() {
    // The harness discovery endpoint: an agent reads this to decide whether a
    // job belongs here at all.
    let h = start().await;
    let body: serde_json::Value = h
        .client
        .get("http://localhost/specs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body[0]["name"], "summarize_docs");
    assert!(body[0]["description"]
        .as_str()
        .unwrap()
        .starts_with("Use when"));
    assert_eq!(body[0]["data_policy"], "local_only");
}
