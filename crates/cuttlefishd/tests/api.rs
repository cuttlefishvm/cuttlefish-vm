//! End-to-end tests against a real daemon on a real unix socket.
//!
//! Nothing here is mocked: an actual `UnixListener`, an actual HTTP client, and
//! an actual wasm guest. That matters most for the SSE test, which only passes
//! because of the replay log — a broadcast-only implementation fails it every
//! time, and would have shipped looking correct.
//!
//! Runs on every platform. The daemon serves over a unix domain socket on unix
//! and a named pipe on Windows, and both go through the same `serve::serve`, so
//! these exercise the real transport wherever they run — including the job that
//! reads a file through the capability check, which is the one most likely to
//! differ across platforms.

use cuttlefish_core::graph::{Branches, NodeGraph};
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
    /// The same `JobStore` the running daemon's `AppState` holds — handed
    /// back so resume tests can mark a job `Interrupted` directly (the same
    /// hand-building pattern `tests/startup.rs` uses for its ledger tests),
    /// without needing a real daemon crash to produce one.
    jobs: JobStore,
    /// Where `submit`/`resume_job` look for a job's on-disk directory —
    /// `$CUTTLEFISH_HOME/jobs`, pointed at this test binary's shared tempdir
    /// by `ensure_test_cuttlefish_home`.
    jobs_root: std::path::PathBuf,
    /// The graph fingerprint this harness's `AppState.checked_nodes`
    /// produces — what a hand-built ledger's `graph_fingerprint` must match
    /// for `resume_job` to accept it.
    fingerprint: String,
}

/// `submit` now resolves a job directory through
/// `cuttlefish_host::ledger::jobs_root()`, which falls back to the real
/// `~/.cuttlefish/jobs` when `$CUTTLEFISH_HOME` is unset — exactly the
/// production behavior, but not something a test suite should ever actually
/// exercise against a developer's real home directory. Point it at one
/// shared tempdir for the whole test binary instead.
///
/// A `OnceLock` plus its own internal `Once`-style init (via
/// `get_or_init`, which itself synchronizes concurrent callers) means every
/// concurrently-running `#[tokio::test]` in this file computes and sets the
/// exact same value at most once, rather than racing distinct
/// `std::env::set_var` calls against each other.
fn ensure_test_cuttlefish_home() {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CUTTLEFISH_HOME", dir.path());
        dir
    });
}

/// A private endpoint for one test.
///
/// Tests run concurrently, so each needs its own. On unix that is a socket
/// inside the test's own tempdir; on Windows a pipe name is not a filesystem
/// path and has no tempdir to live in, so uniqueness comes from the process id
/// plus a counter.
fn unique_endpoint(dir: &std::path::Path) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        dir.join("cf.sock")
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let _ = dir;
        std::path::PathBuf::from(format!(
            r"\\.\pipe\cuttlefishd-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

async fn start() -> Harness {
    start_with_nodes(&["block"]).await
}

/// Same as [`start`], but the served spec's graph has one entry node per name
/// in `names`, each running the same example block against the harness's
/// shared `doc`. Every node has `input: None` (an entry node), so each one
/// independently receives the job's raw input rather than another node's
/// output — enough to build a multi-node graph without needing real
/// inter-node data flow, which the resume tests below don't exercise.
async fn start_with_nodes(names: &[&str]) -> Harness {
    ensure_test_cuttlefish_home();
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, "some document text").unwrap();
    let sock = unique_endpoint(dir.path());

    let spec = Spec {
        name: "summarize_docs".into(),
        description: "Use when a local file needs summarizing.".into(),
        model: ModelRef::new("stub", ""),
        data_policy: DataPolicy::LocalOnly,
        read_roots: vec![dir.path().to_path_buf()],
        nodes: NodeGraph::single("../blocks/echo-summarize".into()),
        branches: Branches::default(),
    };

    // `NodeGraph::single` names its sole node "block"; for a multi-node
    // harness the `nodes` field on `spec` above is unused (only
    // `checked_nodes`/`exclusive_to` on `AppState` drive execution — see
    // `start`'s original comment), so building several `CheckedNode`s
    // directly here, without going through the full resolve pipeline, is
    // consistent with that existing convention.
    let checked_nodes: Vec<cuttlefish_host::dag::CheckedNode> = names
        .iter()
        .map(|name| cuttlefish_host::dag::CheckedNode {
            name: name.to_string(),
            kind: cuttlefish_host::catalog::ArtifactKind::Block,
            resolved: None,
            module_bytes: example_block(),
            signature: cuttlefish_abi::Signature {
                input: cuttlefish_abi::Ty::Json,
                output: cuttlefish_abi::Ty::Json,
            },
            input: None,
            repeat_until: None,
            max_iterations: None,
        })
        .collect();
    let fingerprint = cuttlefish_host::dag::graph_fingerprint(&checked_nodes);

    let jobs = JobStore::default();
    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(cuttlefish_host::infer::StubBackend::default()),
        jobs: jobs.clone(),
        spec: Arc::new(spec),
        checked_nodes: Arc::new(checked_nodes),
        exclusive_to: Arc::new(std::collections::HashMap::new()),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    };

    let sock_for_server = sock.clone();
    tokio::spawn(async move {
        let _ = serve::serve(api::router(state), &sock_for_server, std::future::pending()).await;
    });

    // `as_path()`, not `&sock`: reqwest's sealed provider traits cover `&Path`
    // and `PathBuf` but not `&PathBuf`.
    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(sock.as_path());
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(sock.as_path());
    let client = builder.build().unwrap();

    // Poll the endpoint rather than sleeping a fixed duration, which would be
    // either flaky or slow depending on the machine. Waiting on the *endpoint*
    // rather than a file is what makes this portable: a unix socket appears on
    // the filesystem, a named pipe never does, so the only portable readiness
    // signal is a request that succeeds.
    let mut ready = false;
    for _ in 0..300 {
        if client.get("http://localhost/specs").send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ready,
        "daemon never accepted a request on {}",
        sock.display()
    );

    Harness {
        client,
        _dir: dir,
        doc,
        jobs,
        jobs_root: cuttlefish_host::ledger::jobs_root().expect("CUTTLEFISH_HOME set by test init"),
        fingerprint,
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
async fn a_bare_catalog_name_resolves_through_the_same_path_main_rs_uses() {
    // `main.rs` builds a `Catalog` at `catalog::default_root()`, then for
    // every `spec.nodes` entry calls `pipeline::resolve_and_load(&catalog,
    // spec_dir, &node.block.to_string_lossy(), ResolutionContext::Interactive)`,
    // collects the results into a `HashMap<String, ResolvedInput>` keyed by
    // node name, and hands that to `dag::check_graph`. `start()` above builds
    // `AppState` directly and never exercises that resolution step, so it
    // can't prove a bare catalog name (no `@version`, no path separators)
    // actually works — this test walks the exact same graph shape
    // (`NodeGraph::single`, the one-node graph a bare `block = "...";` spec
    // desugars to) through the exact same two functions, in the exact same
    // order, against a real `Catalog` on a real tempdir.
    use cuttlefish_core::graph::NodeGraph;
    use cuttlefish_host::catalog::{Catalog, ResolutionContext};
    use cuttlefish_host::dag::check_graph;
    use cuttlefish_host::pipeline::resolve_and_load;

    let catalog_dir = tempfile::tempdir().unwrap();
    let spec_dir = tempfile::tempdir().unwrap();
    let engine = wasmtime::Engine::default();

    let wasm_path = spec_dir.path().join("echo-summarize.wasm");
    std::fs::write(&wasm_path, example_block()).unwrap();

    let catalog = Catalog::open(catalog_dir.path());
    catalog
        .add("echo-summarize@1", &wasm_path, &engine)
        .expect("cataloging the compiled example block should succeed");

    let graph = NodeGraph::single(std::path::PathBuf::from("echo-summarize"));
    let resolved: std::collections::HashMap<_, _> = graph
        .nodes
        .iter()
        .map(|(name, node)| {
            let input = resolve_and_load(
                &catalog,
                spec_dir.path(),
                &node.block.to_string_lossy(),
                ResolutionContext::Interactive,
            )
            .expect("a bare name with no @version should resolve to the latest catalog entry");
            (name.clone(), input)
        })
        .collect();
    assert_eq!(
        resolved["block"].resolved.as_deref(),
        Some("echo-summarize@1")
    );

    let checked = check_graph(&engine, &graph, &Branches::default(), &resolved)
        .expect("the resolved single-node graph should typecheck");
    assert_eq!(checked.nodes.len(), 1);
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

/// A job id unlikely to collide with any other test's, even though every
/// test in this file shares one `CUTTLEFISH_HOME`/`jobs_root` tempdir (see
/// `ensure_test_cuttlefish_home`) and tests run concurrently. `submit`
/// itself mints ids from `uuid::Uuid::new_v4()`, which isn't reachable here
/// (`uuid` is a normal, not dev, dependency of `cuttlefishd`) — a
/// nanosecond timestamp is more than precise enough for hand-built job
/// directories that never go through `submit`.
fn unique_job_id(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{nanos}")
}

#[tokio::test]
async fn get_jobs_lists_a_submitted_job() {
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;
    h.await_terminal(&id).await;

    let body: serde_json::Value = h
        .client
        .get("http://localhost/jobs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let jobs = body.as_array().expect("GET /jobs must return an array");
    let found = jobs
        .iter()
        .find(|j| j["job_id"] == id)
        .expect("the submitted job must appear in GET /jobs");
    assert_eq!(found["status"], "completed");
    assert_eq!(found["envelope"]["result"]["summary"], "a stub summary");
}

#[tokio::test]
async fn get_jobs_surfaces_an_interrupted_job() {
    let h = start().await;
    let id = unique_job_id("interrupted");

    // Hand-build a job directory whose ledger was never `finish`ed — the
    // same "still running when the process died" state
    // `tests/startup.rs`'s scan tests construct — then mark it interrupted
    // directly on the harness's `JobStore` (which is the very `JobStore`
    // the running daemon serves), standing in for what the startup scan
    // would have done to a real crashed job.
    let ledger_path = h.jobs_root.join(&id).join("ledger.sqlite");
    {
        let ledger = cuttlefish_host::ledger::Ledger::open(&ledger_path, &h.fingerprint).unwrap();
        assert_eq!(
            ledger.job_status().unwrap(),
            cuttlefish_host::ledger::LedgerJobStatus::Running,
            "sanity check: a freshly opened ledger must start out running"
        );
    }
    h.jobs.mark_interrupted(id.clone()).await;

    let body: serde_json::Value = h
        .client
        .get("http://localhost/jobs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let jobs = body.as_array().expect("GET /jobs must return an array");
    let found = jobs
        .iter()
        .find(|j| j["job_id"] == id)
        .expect("the interrupted job must appear in GET /jobs");
    assert_eq!(found["status"], "interrupted");
    assert!(
        found["envelope"].is_null(),
        "an interrupted job has no envelope yet"
    );
}

#[tokio::test]
async fn resuming_a_non_interrupted_job_is_rejected() {
    let h = start().await;
    let id = h
        .submit_ok(serde_json::json!({ "path": h.doc.to_str().unwrap() }))
        .await;
    let body = h.await_terminal(&id).await;
    assert_eq!(body["status"], "completed");

    let resp = h
        .client
        .post(format!("http://localhost/jobs/{id}/resume"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "resuming a non-Interrupted job must be rejected, not silently re-executed"
    );
}

#[tokio::test]
async fn resuming_an_interrupted_job_completes_remaining_nodes() {
    // Two entry nodes, both fed the job's raw input directly (see
    // `start_with_nodes`'s doc comment) — `n1` is pre-checkpointed with an
    // obviously-fake output; `n2` is left uncheckpointed. The ledger's
    // resume-aware loop (`runner::run_job`) skips `n1` by replaying the
    // fake cached value, but `n2` has no checkpoint at all, so it can only
    // reach a terminal state by actually invoking the wasm block — which
    // the stub backend answers with a *specific*, known string
    // ("a stub summary") that the hand-written fake value never contains.
    // If resume were broken and just synthesized results for
    // not-yet-checkpointed nodes instead of really running them, the final
    // result would not carry that string.
    let h = start_with_nodes(&["n1", "n2"]).await;
    let id = unique_job_id("resume-complete");
    let job_dir = h.jobs_root.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(
        job_dir.join("input.json"),
        serde_json::json!({ "path": h.doc.to_str().unwrap() }).to_string(),
    )
    .unwrap();

    {
        let ledger =
            cuttlefish_host::ledger::Ledger::open(&job_dir.join("ledger.sqlite"), &h.fingerprint)
                .unwrap();
        ledger
            .write_completed(
                "n1",
                &serde_json::json!({ "summary": "PRE-CACHED-STALE", "marker": "stale" }),
            )
            .unwrap();
        // `n2` is deliberately left without a checkpoint, and the ledger is
        // never `finish`ed — exactly the mid-flight state the startup scan
        // exists to detect.
    }
    h.jobs.mark_interrupted(id.clone()).await;

    let resp = h
        .client
        .post(format!("http://localhost/jobs/{id}/resume"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let body = h.await_terminal(&id).await;
    assert_eq!(body["status"], "completed");
    // The job's result is the last node's output in topological order —
    // `n2` here — so this can only be true if `n2` actually ran through the
    // wasm block rather than being synthesized or skipped.
    assert_eq!(
        body["envelope"]["result"]["summary"], "a stub summary",
        "n2 must have genuinely executed, not been replayed from a stale cache: {body}"
    );
    assert_ne!(
        body["envelope"]["result"]["summary"], "PRE-CACHED-STALE",
        "the final result must not be n1's stale cached value"
    );

    // The above only proves `n2` genuinely ran — `n2` was always going to be
    // the job's result regardless of whether `n1`'s skip-on-resume logic
    // actually worked, since it's the last non-skipped node in topological
    // order either way. Prove `n1` specifically was skipped, not silently
    // re-run, by reopening the ledger and checking its checkpoint is still
    // the exact stale value written before resume — a real re-execution
    // would have overwritten it with the stub backend's real output.
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&job_dir.join("ledger.sqlite"), &h.fingerprint)
            .unwrap();
    assert_eq!(
        ledger.get_completed("n1").unwrap(),
        Some(serde_json::json!({ "summary": "PRE-CACHED-STALE", "marker": "stale" })),
        "n1's checkpoint must still be the pre-seeded stale value — proving it was skipped on \
         resume rather than silently re-executed and re-checkpointed"
    );
}

#[tokio::test]
async fn resuming_a_job_whose_ledger_fingerprint_does_not_match_is_refused() {
    let h = start().await;
    let id = unique_job_id("bad-fingerprint");
    let job_dir = h.jobs_root.join(&id);
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(
        job_dir.join("input.json"),
        serde_json::json!({ "path": h.doc.to_str().unwrap() }).to_string(),
    )
    .unwrap();

    // A ledger whose recorded fingerprint deliberately does not match what
    // `dag::graph_fingerprint(&st.checked_nodes)` computes for the spec
    // this harness actually serves — standing in for a job that was
    // started against a different version of the spec than the daemon is
    // now running.
    {
        let ledger = cuttlefish_host::ledger::Ledger::open(
            &job_dir.join("ledger.sqlite"),
            "not-a-real-fingerprint",
        )
        .unwrap();
        assert_eq!(
            ledger.job_status().unwrap(),
            cuttlefish_host::ledger::LedgerJobStatus::Running
        );
    }
    h.jobs.mark_interrupted(id.clone()).await;

    let resp = h
        .client
        .post(format!("http://localhost/jobs/{id}/resume"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]
        .as_str()
        .expect("a rejection must explain itself")
        .to_lowercase();
    assert!(
        msg.contains("fingerprint"),
        "rejection must name the graph fingerprint mismatch: {msg}"
    );

    // No node must have run: the ledger still has no checkpoint at all.
    let ledger = cuttlefish_host::ledger::Ledger::open(
        &job_dir.join("ledger.sqlite"),
        "not-a-real-fingerprint",
    )
    .unwrap();
    assert!(
        ledger.get_completed("block").unwrap().is_none(),
        "a fingerprint-mismatched job must not have run any node"
    );
}

#[tokio::test]
async fn shutdown_causes_serve_to_return() {
    // Same manual `AppState`/`serve::serve` setup `start_with_nodes` uses,
    // but this test needs the `serve` future itself (not just the running
    // task behind `start`'s helpers) so it can assert the *task* — not just
    // a subsequent request — actually completes once `/shutdown` is called.
    ensure_test_cuttlefish_home();
    let dir = tempfile::tempdir().unwrap();
    let sock = unique_endpoint(dir.path());

    let spec = Spec {
        name: "summarize_docs".into(),
        description: "Use when a local file needs summarizing.".into(),
        model: ModelRef::new("stub", ""),
        data_policy: DataPolicy::LocalOnly,
        read_roots: vec![dir.path().to_path_buf()],
        nodes: NodeGraph::single("../blocks/echo-summarize".into()),
        branches: Branches::default(),
    };

    let checked_nodes = vec![cuttlefish_host::dag::CheckedNode {
        name: "block".to_string(),
        kind: cuttlefish_host::catalog::ArtifactKind::Block,
        resolved: None,
        module_bytes: example_block(),
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            output: cuttlefish_abi::Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
    }];

    // A real `Notify`, distinct from the `std::future::pending()` every other
    // test in this file uses — this is the one test that actually needs to
    // trigger shutdown rather than merely tolerate the parameter existing.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(cuttlefish_host::infer::StubBackend::default()),
        jobs: JobStore::default(),
        spec: Arc::new(spec),
        checked_nodes: Arc::new(checked_nodes),
        exclusive_to: Arc::new(std::collections::HashMap::new()),
        shutdown: shutdown.clone(),
    };

    let sock_for_server = sock.clone();
    let shutdown_for_serve = shutdown.clone();
    let handle = tokio::spawn(async move {
        serve::serve(api::router(state), &sock_for_server, async move {
            shutdown_for_serve.notified().await
        })
        .await
    });

    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(sock.as_path());
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(sock.as_path());
    let client = builder.build().unwrap();

    let mut ready = false;
    for _ in 0..300 {
        if client.get("http://localhost/specs").send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ready,
        "daemon never accepted a request on {}",
        sock.display()
    );

    let resp = client
        .post("http://localhost/shutdown")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // The real assertion: the spawned `serve` task actually returns, rather
    // than hanging forever — a timeout here means graceful shutdown never
    // reached `axum::serve`'s `with_graceful_shutdown` future.
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("serve task did not complete after POST /shutdown")
        .expect("serve task panicked")
        .expect("serve returned an error");
}

#[tokio::test]
async fn shutdown_force_exits_even_with_an_open_sse_stream() {
    // The gap the force-exit fallback closes: `job_events` keeps its stream
    // open for as long as the store holds a live sender — which is forever,
    // not just until the job finishes (see `streams_tokens_and_a_result_over_sse`'s
    // comment). axum's graceful shutdown only closes a connection once its
    // current response finishes, so a client sitting on that stream would
    // otherwise block `/shutdown` indefinitely.
    //
    // Proving the process itself goes down needs an actual OS process —
    // every other test in this file drives `serve::serve` in-process, which
    // can prove a *future* resolves but can't prove a real `exit(0)` fires
    // without also ending this test binary and every test running alongside
    // it. So, uniquely among this file's tests, this spawns the genuine
    // `cuttlefishd` binary.
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sock = unique_endpoint(dir.path());

    std::fs::write(dir.path().join("block.wasm"), example_block()).unwrap();
    std::fs::write(dir.path().join("doc.txt"), "some document text").unwrap();
    let spec_path = dir.path().join("spec.cf");
    std::fs::write(
        &spec_path,
        r#"
spec summarize_docs = {
  description = "Use when a local file needs summarizing.";
  model = Stub "";
  data_policy = Local_only;
  capabilities = [ Read "." ];
  block = "block.wasm";
}
"#,
    )
    .unwrap();

    // `CUTTLEFISH_SHUTDOWN_GRACE_MS` shrinks the force-exit fallback's wait
    // to something a test can afford to sit through; production never sets
    // it, so this changes nothing about the constant a real daemon uses.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefishd"))
        .arg(&spec_path)
        .arg(dir.path().join("block.wasm"))
        .arg(&sock)
        .env("CUTTLEFISH_HOME", home.path())
        .env("CUTTLEFISH_SHUTDOWN_GRACE_MS", "200")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the cuttlefishd binary under test must be spawnable");

    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(sock.as_path());
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(sock.as_path());
    let client = builder.build().unwrap();

    let mut ready = false;
    for _ in 0..500 {
        if client.get("http://localhost/specs").send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if !ready {
        let _ = child.kill();
        panic!("daemon never accepted a request on {}", sock.display());
    }

    let id = client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({
            "spec": "summarize_docs",
            "input": { "path": dir.path().join("doc.txt").to_str().unwrap() },
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["job_id"]
        .as_str()
        .expect("submission must return a job_id")
        .to_string();

    // Open the event stream and hold onto the response without ever reading
    // or dropping it — dropping it would close the connection from the
    // client side and let axum's ordinary graceful path succeed on its own,
    // which would prove nothing about the fallback. It stays alive (and the
    // connection stays open) for the rest of this function.
    let events = client
        .get(format!("http://localhost/jobs/{id}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);

    let resp = client
        .post("http://localhost/shutdown")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // The real assertion: the daemon *process* actually exits — well within
    // the 200ms grace period plus slack — despite the still-open SSE
    // connection `events` is holding. Without the force-exit fallback this
    // would hang until `events` is dropped, which for a real long-running
    // job could be indefinite; this loop would instead spin for the whole
    // deadline below and then fail, exactly the regression this test guards.
    let mut exited = None;
    for _ in 0..500 {
        if let Some(status) = child.try_wait().expect("polling the child process") {
            exited = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    match exited {
        Some(status) => assert!(status.success(), "cuttlefishd exited non-zero: {status:?}"),
        None => {
            let _ = child.kill();
            panic!(
                "cuttlefishd did not exit within ~5s of POST /shutdown despite an open SSE \
                 stream — the force-exit fallback did not fire"
            );
        }
    }

    // Keep the connection alive up to here, so the exit above is proven
    // against a genuinely still-open stream rather than one this function
    // already let close.
    drop(events);
}

#[tokio::test]
async fn concurrent_resume_calls_only_spawn_one_run() {
    // The bug this guards against: two `POST /jobs/{id}/resume` calls that
    // both read `status == Interrupted` via the plain, non-atomic pre-flight
    // check (nothing updates status in between, before this fix existed)
    // would *both* go on to open the ledger and spawn `run_job` — a genuine
    // double-execution race against the same sqlite file.
    //
    // Exercising this truly at the HTTP layer is not honest: `resume_job`
    // does a lot of synchronous work (std::fs, rusqlite) between the
    // pre-flight check and the atomic guard, and a `#[tokio::test]`'s
    // current-thread runtime only switches tasks at await points, so two
    // `tokio::join!`ed HTTP requests aren't guaranteed to actually interleave
    // at the moment that matters. What's actually being guaranteed is a
    // property of `JobStore::try_start_resume` itself: driven concurrently
    // via `tokio::join!`, at most one of two calls against the same id may
    // observe `Interrupted` and win the transition to `Running`. Testing
    // that directly is a more reliable proof of the guarantee than hoping an
    // HTTP-level race lands on the right instruction.
    let jobs = JobStore::default();
    let id = "race-job".to_string();
    jobs.mark_interrupted(id.clone()).await;

    // Stand in for both concurrent handlers' earlier, non-atomic
    // `status == Interrupted` pre-flight check having already passed for
    // both of them — exactly the window that made the original bug
    // possible.
    assert_eq!(
        jobs.get(&id).await.unwrap().status,
        cuttlefish_abi::JobStatus::Interrupted
    );
    assert_eq!(
        jobs.get(&id).await.unwrap().status,
        cuttlefish_abi::JobStatus::Interrupted
    );

    // Now both race for the one atomic transition that's allowed to actually
    // spawn `run_job`, driven concurrently rather than called sequentially
    // in sequence, so this proves the guarantee under real interleaving
    // rather than under an ordering the test itself imposed.
    let (a, b) = tokio::join!(jobs.try_start_resume(&id), jobs.try_start_resume(&id));

    let wins = [a.is_some(), b.is_some()]
        .into_iter()
        .filter(|won| *won)
        .count();
    assert_eq!(
        wins,
        1,
        "exactly one of two concurrent resume attempts must win the race \
         (a={}, b={})",
        a.is_some(),
        b.is_some()
    );
    let winner = a.or(b).expect("one call must have won");
    assert_eq!(
        winner.status,
        cuttlefish_abi::JobStatus::Running,
        "the winner's job must actually be flipped to Running"
    );

    // The guard stays exclusive after the race too — a third caller arriving
    // later must still be refused, not slip through because the two racers
    // "used up" some one-shot state incorrectly.
    assert!(jobs.try_start_resume(&id).await.is_none());
}
