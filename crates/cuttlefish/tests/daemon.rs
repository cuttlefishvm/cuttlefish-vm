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

#[tokio::test]
async fn submit_returns_a_job_id_immediately_without_waiting_for_completion() {
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = dir.path().join("daemon.sock");
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
