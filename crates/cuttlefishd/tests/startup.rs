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
