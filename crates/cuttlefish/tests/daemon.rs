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

#[tokio::test]
async fn submit_returns_a_job_id_immediately_without_waiting_for_completion() {
    support::ensure_test_cuttlefish_home();

    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.cuttlefish");
    std::fs::write(&spec_path, submit_test_spec_src()).unwrap();
    std::fs::write(dir.path().join("block.wasm"), support::example_block()).unwrap();

    let endpoint = dir.path().join("daemon.sock");
    let mut child = support::spawn_daemon(&spec_path, &endpoint).await;

    // Run `cuttlefish submit` on a blocking thread so a hang (e.g. if
    // `submit` accidentally polled to completion like `run` does) shows up
    // as a bounded timeout failure rather than an indefinitely hung test.
    let endpoint_for_cmd = endpoint.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefish"))
                .args(["submit", "--endpoint"])
                .arg(&endpoint_for_cmd)
                .args(["--spec", "submit_test", "--input", "{}"])
                .output()
                .expect("cuttlefish submit failed to run")
        }),
    )
    .await;

    let _ = child.kill();

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
}
