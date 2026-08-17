//! Tests for the daemon-startup scan that finds jobs whose ledger says they
//! were still running when the process last died.
//!
//! These call `cuttlefishd::scan_for_interrupted_jobs` directly against a
//! hand-built `JobStore`, the same way `tests/api.rs`'s `start()` helper
//! builds an `AppState`/`JobStore` without going through `main()` — there is
//! no daemon here at all, just the scan function and a real ledger on disk.

use cuttlefish_abi::JobStatus;
use cuttlefish_host::ledger::Ledger;
use cuttlefishd::state::JobStore;

#[tokio::test]
async fn a_job_directory_with_status_running_is_marked_interrupted_on_scan() {
    let jobs_root = tempfile::tempdir().unwrap();
    let job_id = "some-job-id";
    let ledger_path = jobs_root.path().join(job_id).join("ledger.sqlite");

    // `Ledger::open` sets `status = 'running'` the first time a ledger is
    // created and touches nothing else until `finish` is called — so a
    // freshly opened ledger that is dropped without ever calling `finish`
    // is exactly the "still running when the process died" state this scan
    // exists to detect. Running a real job through `run_job` instead would
    // leave the ledger `finish`ed (completed/failed/cancelled), not running.
    {
        let ledger = Ledger::open(&ledger_path, "some-fingerprint").unwrap();
        assert_eq!(
            ledger.job_status().unwrap(),
            cuttlefish_host::ledger::LedgerJobStatus::Running,
            "sanity check: a freshly opened ledger must start out running"
        );
    }

    let jobs = JobStore::default();
    cuttlefishd::scan_for_interrupted_jobs(jobs_root.path(), "some-fingerprint", &jobs)
        .await
        .unwrap();

    let job = jobs
        .get(job_id)
        .await
        .expect("the scan should have inserted a job for the running ledger");
    assert_eq!(job.status, JobStatus::Interrupted);
    assert!(
        job.envelope.is_none(),
        "a merely-interrupted job has no envelope yet — resume is what gives it one"
    );
}

#[tokio::test]
async fn a_finished_job_is_not_marked_interrupted() {
    let jobs_root = tempfile::tempdir().unwrap();
    let job_id = "finished-job";
    let ledger_path = jobs_root.path().join(job_id).join("ledger.sqlite");

    {
        let ledger = Ledger::open(&ledger_path, "some-fingerprint").unwrap();
        ledger.finish("completed").unwrap();
    }

    let jobs = JobStore::default();
    cuttlefishd::scan_for_interrupted_jobs(jobs_root.path(), "some-fingerprint", &jobs)
        .await
        .unwrap();

    assert!(
        jobs.get(job_id).await.is_none(),
        "a finished job must not be reported as interrupted"
    );
}

#[tokio::test]
async fn a_missing_jobs_root_is_not_an_error() {
    let jobs_root = tempfile::tempdir().unwrap();
    let missing = jobs_root.path().join("does-not-exist");

    let jobs = JobStore::default();
    cuttlefishd::scan_for_interrupted_jobs(&missing, "some-fingerprint", &jobs)
        .await
        .expect("a jobs_root that doesn't exist yet (fresh install) must not error");
}

#[tokio::test]
async fn one_corrupted_ledger_does_not_abort_the_whole_scan() {
    let jobs_root = tempfile::tempdir().unwrap();

    // A genuinely healthy job, still running.
    let healthy_id = "healthy-job";
    let healthy_ledger_path = jobs_root.path().join(healthy_id).join("ledger.sqlite");
    {
        let ledger = Ledger::open(&healthy_ledger_path, "some-fingerprint").unwrap();
        assert_eq!(
            ledger.job_status().unwrap(),
            cuttlefish_host::ledger::LedgerJobStatus::Running,
        );
    }

    // A job directory whose ledger file is garbage — e.g. from a hard crash
    // mid-write. Written directly as bytes rather than through
    // `Ledger::open`, so it is not a valid sqlite database at all.
    let corrupted_id = "corrupted-job";
    let corrupted_dir = jobs_root.path().join(corrupted_id);
    std::fs::create_dir_all(&corrupted_dir).unwrap();
    std::fs::write(
        corrupted_dir.join("ledger.sqlite"),
        b"not a sqlite database",
    )
    .unwrap();

    let jobs = JobStore::default();
    cuttlefishd::scan_for_interrupted_jobs(jobs_root.path(), "some-fingerprint", &jobs)
        .await
        .expect(
            "one corrupted ledger must not fail the whole scan — that would brick the daemon \
             on exactly the crash it exists to detect",
        );

    let healthy_job = jobs
        .get(healthy_id)
        .await
        .expect("the healthy job must still be discovered and marked interrupted");
    assert_eq!(healthy_job.status, JobStatus::Interrupted);

    assert!(
        jobs.get(corrupted_id).await.is_none(),
        "a corrupted ledger should be skipped, not recorded as any particular status"
    );
}

#[tokio::test]
async fn cancelling_an_interrupted_job_transitions_it_to_cancelled() {
    let jobs = JobStore::default();
    let job_id = "interrupted-job".to_string();
    jobs.mark_interrupted(job_id.clone()).await;

    let before = jobs.get(&job_id).await.unwrap();
    assert_eq!(before.status, JobStatus::Interrupted);

    let existed = jobs.cancel(&job_id).await;
    assert!(existed, "cancel must report that the job exists");

    let after = jobs.get(&job_id).await.unwrap();
    assert_eq!(
        after.status,
        JobStatus::Cancelled,
        "an interrupted job's cancel must actually transition it to Cancelled, not leave it \
         stuck at Interrupted while reporting success"
    );
}

/// `JobStore::cancel` resolves a job's on-disk directory through
/// `cuttlefish_host::ledger::jobs_root()`, which falls back to the real
/// `~/.cuttlefish/jobs` when `$CUTTLEFISH_HOME` is unset — exactly the
/// production behavior, but not something a test suite should ever actually
/// exercise against a developer's real home directory. Point it at one
/// shared tempdir for the whole test binary instead, the same pattern
/// `tests/api.rs`'s `ensure_test_cuttlefish_home` uses.
///
/// A `OnceLock` plus its own internal `Once`-style init (via `get_or_init`,
/// which itself synchronizes concurrent callers) means every concurrently
/// running `#[tokio::test]` in this file computes and sets the exact same
/// value at most once, rather than racing distinct `std::env::set_var`
/// calls against each other. No other test in this file reads or depends on
/// `CUTTLEFISH_HOME`, so this is safe to set unconditionally.
///
/// Returns `jobs_root()` itself (i.e. `$CUTTLEFISH_HOME/jobs`), not
/// `$CUTTLEFISH_HOME` — matching what `JobStore::cancel` actually resolves
/// job directories under.
fn ensure_test_cuttlefish_home() -> std::path::PathBuf {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CUTTLEFISH_HOME", dir.path());
        dir
    });
    cuttlefish_host::ledger::jobs_root().expect("CUTTLEFISH_HOME set above")
}

#[tokio::test]
async fn cancelling_an_interrupted_job_persists_to_its_ledger() {
    // The bug this guards against: `cancel`'s special case for an
    // `Interrupted` job used to transition `job.status` to `Cancelled`
    // in-memory only, leaving the ledger's `job_status` table still saying
    // `'running'`. A daemon restart before anything else resolved the job
    // would have the startup scan re-read that stale `'running'` row and
    // silently re-mark the job `Interrupted`, discarding the cancel
    // decision with no error and no signal. Proving the in-memory status
    // looks right (as the sibling test above does) isn't enough — this
    // reopens the ledger directly afterward, the same way the startup scan
    // would on a real restart, and checks what it actually persisted.
    let jobs_root = ensure_test_cuttlefish_home();
    let job_id = "interrupted-job-to-cancel";
    let ledger_path = jobs_root.join(job_id).join("ledger.sqlite");

    // A ledger left `running` — exactly the on-disk state an interrupted
    // job has: created, never `finish`ed.
    {
        let ledger = Ledger::open(&ledger_path, "some-fingerprint").unwrap();
        assert_eq!(
            ledger.job_status().unwrap(),
            cuttlefish_host::ledger::LedgerJobStatus::Running,
            "sanity check: a freshly opened ledger must start out running"
        );
    }

    let jobs = JobStore::default();
    jobs.mark_interrupted(job_id.to_string()).await;

    let existed = jobs.cancel(job_id).await;
    assert!(existed, "cancel must report that the job exists");

    let after = jobs.get(job_id).await.unwrap();
    assert_eq!(after.status, JobStatus::Cancelled);

    // Reopen the ledger fresh, as a restarted daemon's startup scan would,
    // and confirm the on-disk record now matches the in-memory one.
    let reopened = Ledger::open(&ledger_path, "some-fingerprint").unwrap();
    assert_eq!(
        reopened.job_status().unwrap(),
        cuttlefish_host::ledger::LedgerJobStatus::Cancelled,
        "cancelling an interrupted job must persist to its ledger — otherwise a restart before \
         this job's disposition is otherwise resolved would see 'running' and silently re-mark \
         it Interrupted, discarding the cancel decision"
    );
}

/// A binary spawned as a render worker must recognise that, even when it
/// cannot render.
///
/// The failure this pins: `cuttlefish-host` can carry `pdf-render` while the
/// daemon binary does not, because they are separate features and enabling
/// only the host's compiles fine. Rendering then spawns this binary as a
/// worker, the worker argument goes unrecognised, and the daemon parses it
/// as its own arguments — answering a render request with usage text, which
/// surfaces as a per-item job failure reading `Error: usage: cuttlefishd
/// <spec> ...`. That names neither rendering nor the feature.
#[test]
fn a_render_worker_invocation_is_never_mistaken_for_a_usage_error() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cuttlefishd"))
        .args([
            "--__cuttlefish_render_worker",
            "/nonexistent.pdf",
            "0",
            "256",
        ])
        .output()
        .expect("cuttlefishd should run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("usage: cuttlefishd"),
        "a worker invocation must never fall through to argument parsing: {combined}"
    );
    // Whether it renders or refuses depends on the feature; either way it
    // must speak about rendering.
    assert!(
        combined.to_lowercase().contains("render") || combined.contains("pdfium"),
        "the failure must name rendering: {combined}"
    );
}
