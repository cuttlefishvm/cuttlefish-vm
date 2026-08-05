//! End-to-end tests of `cuttlefish`'s subcommands against a real, spawned
//! `cuttlefishd` process. Nothing mocked: a real unix socket (named pipe on
//! Windows), a real wasm block, a real client binary.

mod support;

/// The spec text every test below shares: a single entry named `submit_test`
/// whose block is a bare `block.wasm` file beside the spec. `capabilities =
/// [ ]` (no grants at all) is fine here — this suite never waits for the job
/// to actually run, only for the daemon to *accept* the submission, so the
/// job's own runtime behavior (and whether it fails for lack of a capability)
/// is irrelevant.
fn submit_test_spec_src() -> &'static str {
    r#"
spec submit_test = {
  description = "Use when testing cuttlefish submit.";
  model = Stub "";
  data_policy = Local_only;
  capabilities = [ ];
  block = "block.wasm";
}
"#
}

/// Run `cuttlefish submit --endpoint <endpoint> --spec submit_test --input
/// {}` against an already-running daemon and return its output.
fn run_submit(endpoint: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["submit", "--endpoint"])
        .arg(endpoint)
        .args(["--spec", "submit_test", "--input", "{}"])
        .output()
        .expect("cuttlefish submit failed to run")
}

/// Run `cuttlefish submit --endpoint <endpoint> --spec <spec> --input
/// <input>` against an already-running daemon and return its output. Unlike
/// `run_submit`, lets the caller pick the spec and input — needed by
/// `cancel_stops_a_job`, which submits against a different spec than the
/// other tests in this file.
fn run_submit_with(endpoint: &std::path::Path, spec: &str, input: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["submit", "--endpoint"])
        .arg(endpoint)
        .args(["--spec", spec, "--input", input])
        .output()
        .expect("cuttlefish submit failed to run")
}

/// Run `cuttlefish jobs --endpoint <endpoint>` and return its output.
fn run_jobs(endpoint: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["jobs", "--endpoint"])
        .arg(endpoint)
        .output()
        .expect("cuttlefish jobs failed to run")
}

/// Run `cuttlefish resume --endpoint <endpoint> <job_id>` and return its
/// output.
fn run_resume(endpoint: &std::path::Path, job_id: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["resume", "--endpoint"])
        .arg(endpoint)
        .arg(job_id)
        .output()
        .expect("cuttlefish resume failed to run")
}

/// Run `cuttlefish cancel --endpoint <endpoint> <job_id>` and return its
/// output.
fn run_cancel(endpoint: &std::path::Path, job_id: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["cancel", "--endpoint"])
        .arg(endpoint)
        .arg(job_id)
        .output()
        .expect("cuttlefish cancel failed to run")
}

/// Run `cuttlefish shutdown --endpoint <endpoint>` and return its output.
fn run_shutdown(endpoint: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
        .args(["shutdown", "--endpoint"])
        .arg(endpoint)
        .output()
        .expect("cuttlefish shutdown failed to run")
}

/// A single node spec whose node loops on `repeat_until` for effectively
/// forever (`max_iterations` is astronomically large, and the field it
/// watches — `summary` — never comes back as the literal string `"done"`
/// from a stub-backed `echo-summarize` block). Exists solely to give
/// `cancel_stops_a_job` a job that is still genuinely running by the time a
/// freshly spawned `cuttlefish cancel` process can reach it: the trivial
/// one-shot `submit_test` spec finishes far too fast (sub-millisecond, per
/// `run_stage`'s per-`Command`-step cancellation checks) for a real spawned
/// CLI process — process start, socket connect, HTTP round trip — to
/// reliably race a `DELETE` in ahead of completion. Reuses the same
/// `echo-summarize` block every other test in this file already builds
/// (`support::example_block`), rather than compiling a bespoke slow block
/// the way `crates/cuttlefish-host/tests/support/mod.rs`'s
/// `block_with_source` does — that machinery exists for tests that need a
/// block with custom *behavior*, not merely one that runs a long time, and
/// spinning up a whole extra cargo build per test run is not warranted here
/// when a large `max_iterations` gets the same guarantee far more cheaply.
fn cancel_test_spec_src() -> &'static str {
    r#"
spec cancel_test = {
  description = "Use when testing cuttlefish cancel.";
  model = Stub "";
  data_policy = Local_only;
  capabilities = [ Read "." ];
  nodes = {
    block = { block = "block.wasm"; repeat_until = "summary"; max_iterations = 1000000000; };
  };
}
"#
}

#[tokio::test]
async fn submit_returns_a_job_id_immediately_without_waiting_for_completion() {
    // Held for this test's entire body — see `daemon_test_guard`'s doc
    // comment for why these tests can't cold-start their daemons
    // concurrently on a contended CI runner.
    let _daemon_guard = support::daemon_test_guard().await;
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = support::unique_endpoint(dir.path());
    let mut daemon = support::spawn_daemon(&spec_path, &endpoint).await;

    // A daemon's *first-ever* job submission pays a one-time cold cost (the
    // job's ledger/sqlite file gets created from scratch) that can run into
    // the hundreds of milliseconds on its own — dwarfing anything this test
    // wants to measure below. Absorb that cost on a throwaway job before
    // timing anything, so the timed submission below only ever measures
    // `submit`'s own behavior.
    let warmup_endpoint = endpoint.clone();
    tokio::task::spawn_blocking(move || run_submit(&warmup_endpoint))
        .await
        .unwrap();

    // Run `cuttlefish submit` on a blocking thread, timed, so a hang (e.g. if
    // `submit` accidentally polled to completion like `run` does) shows up as
    // a bounded timeout failure rather than an indefinitely hung test.
    let endpoint_for_cmd = endpoint.clone();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || run_submit(&endpoint_for_cmd)),
    )
    .await;
    let elapsed = started.elapsed();

    // Explicit at this test's natural end for clarity, though redundant with
    // `DaemonGuard`'s `Drop` — which is what actually protects the panics
    // above (e.g. the warm-up call's `.unwrap()`) from leaking the process.
    daemon.kill();

    let output = result
        .expect(
            "`cuttlefish submit` did not return within 10s — it may have blocked waiting for \
             the job to finish, like `run` does, instead of returning immediately",
        )
        .expect("the blocking task panicked");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let job_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !job_id.is_empty(),
        "expected a job_id on stdout, got nothing"
    );
    assert!(
        uuid::Uuid::parse_str(&job_id).is_ok(),
        "stdout wasn't a UUID: {job_id}"
    );

    // The real assertion: `submit` must return in a bounded, small amount of
    // time, independent of how long the submitted job itself takes to run —
    // it posts the job and prints, it never polls. `run`'s polling loop, by
    // contrast, has to wait for the job to actually execute (real wasm
    // module compilation and instantiation through wasmtime, plus at least
    // one `POLL_INTERVAL` sleep), which is real, non-negligible work.
    //
    // This bound was picked empirically against this repo's own timings, not
    // guessed: on this machine, after the warm-up above, a *correct* `submit`
    // consistently completed in 33-137ms across nine runs, while temporarily
    // reintroducing the exact bug this test guards against (making `submit`
    // poll `GET /jobs/{id}` to a terminal state before printing, i.e. copying
    // `run`'s loop) consistently pushed it to 295-341ms across three runs — a
    // clean, reproducible gap. 250ms sits in the middle of that gap with
    // margin on both sides (113ms above the correct ceiling observed, 45ms
    // below the buggy floor observed).
    assert!(
        elapsed < std::time::Duration::from_millis(250),
        "`cuttlefish submit` took {elapsed:?}, which is far more than a bare POST + print \
         should ever take — it may have started waiting on the job's own completion again"
    );
}

#[tokio::test]
async fn jobs_lists_a_submitted_job() {
    let _daemon_guard = support::daemon_test_guard().await;
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = support::unique_endpoint(dir.path());
    let mut daemon = support::spawn_daemon(&spec_path, &endpoint).await;

    let submit_output = run_submit(&endpoint);
    assert!(
        submit_output.status.success(),
        "{}",
        String::from_utf8_lossy(&submit_output.stderr)
    );
    let job_id = String::from_utf8_lossy(&submit_output.stdout)
        .trim()
        .to_string();

    let jobs_output = run_jobs(&endpoint);
    daemon.kill();

    assert!(
        jobs_output.status.success(),
        "{}",
        String::from_utf8_lossy(&jobs_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&jobs_output.stdout);
    assert!(
        stdout.contains(&job_id),
        "expected job_id {job_id} to appear in `cuttlefish jobs` output:\n{stdout}"
    );
}

#[tokio::test]
async fn resume_on_a_non_interrupted_job_reports_the_daemons_rejection() {
    let _daemon_guard = support::daemon_test_guard().await;
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = support::unique_endpoint(dir.path());
    let mut daemon = support::spawn_daemon(&spec_path, &endpoint).await;

    let submit_output = run_submit(&endpoint);
    assert!(
        submit_output.status.success(),
        "{}",
        String::from_utf8_lossy(&submit_output.stderr)
    );
    let job_id = String::from_utf8_lossy(&submit_output.stdout)
        .trim()
        .to_string();

    // No wait between submit and resume: `submit_job`'s `st.jobs.insert`
    // happens before the daemon's 202 response is even sent, so the job is
    // guaranteed to exist by the time this runs — it just can't possibly be
    // `Interrupted` (nothing crashed), so the daemon's rejection is
    // deterministic rather than a race against the job's own completion.
    let resume_output = run_resume(&endpoint, &job_id);
    daemon.kill();

    assert!(
        !resume_output.status.success(),
        "expected `cuttlefish resume` to exit non-zero for a non-Interrupted job"
    );
    let stderr = String::from_utf8_lossy(&resume_output.stderr);
    assert!(
        stderr.contains("Interrupted"),
        "expected the daemon's rejection message to surface on stderr, got:\n{stderr}"
    );
}

#[tokio::test]
async fn cancel_stops_a_job() {
    let _daemon_guard = support::daemon_test_guard().await;
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, cancel_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();
    std::fs::write(dir.path().join("doc.txt"), "some document text").unwrap();

    let endpoint = support::unique_endpoint(dir.path());
    let mut daemon = support::spawn_daemon(&spec_path, &endpoint).await;

    let input =
        serde_json::json!({ "path": dir.path().join("doc.txt").to_str().unwrap() }).to_string();
    let submit_output = run_submit_with(&endpoint, "cancel_test", &input);
    assert!(
        submit_output.status.success(),
        "{}",
        String::from_utf8_lossy(&submit_output.stderr)
    );
    let job_id = String::from_utf8_lossy(&submit_output.stdout)
        .trim()
        .to_string();

    let cancel_output = run_cancel(&endpoint, &job_id);
    assert!(
        cancel_output.status.success(),
        "{}",
        String::from_utf8_lossy(&cancel_output.stderr)
    );

    // Poll `GET /jobs/{id}` directly — there's no `cuttlefish` subcommand
    // that surfaces a single job's status, only `run` (which polls to
    // completion) and `jobs` (the whole list) — bounded, so a cancel that
    // silently failed to take effect shows up as a timeout rather than an
    // indefinite hang.
    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(endpoint.as_path());
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(endpoint.as_path());
    let client = builder.build().unwrap();

    let mut status = None;
    for _ in 0..500 {
        let body: serde_json::Value = client
            .get(format!("http://localhost/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let s = body["status"].as_str().unwrap_or("").to_string();
        if matches!(s.as_str(), "cancelled" | "completed" | "failed") {
            status = Some(s);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    daemon.kill();

    assert_eq!(
        status.as_deref(),
        Some("cancelled"),
        "job did not settle on `cancelled` within the bounded wait (got {status:?}) — \
         `cuttlefish cancel` may not have actually reached the daemon"
    );
}

#[tokio::test]
async fn shutdown_causes_the_daemon_process_to_exit() {
    let _daemon_guard = support::daemon_test_guard().await;
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = support::unique_endpoint(dir.path());
    let mut daemon = support::spawn_daemon(&spec_path, &endpoint).await;

    let shutdown_output = run_shutdown(&endpoint);
    assert!(
        shutdown_output.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown_output.stderr)
    );

    // The real assertion: the daemon *process* exits on its own within a
    // bounded wait — no `daemon.kill()` needed here. Needing one would mean
    // `cuttlefish shutdown` didn't actually cause the process to go down.
    let mut exited = false;
    for _ in 0..500 {
        if daemon
            .try_wait()
            .expect("polling the daemon process")
            .is_some()
        {
            exited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        exited,
        "cuttlefishd did not exit within the bounded wait after `cuttlefish shutdown`"
    );
}
