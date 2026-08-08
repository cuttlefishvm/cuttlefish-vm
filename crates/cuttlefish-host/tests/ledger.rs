use cuttlefish_host::ledger::Ledger;
use std::sync::Mutex;

// Env vars are process-global; serialize the two tests that touch
// CUTTLEFISH_JOBS_HOME/CUTTLEFISH_HOME so they can't interleave and read
// each other's value.
static ENV_GUARD: Mutex<()> = Mutex::new(());

#[test]
fn a_fresh_ledger_has_no_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "test-fingerprint").unwrap();
    assert_eq!(ledger.get_completed("some_node").unwrap(), None);
}

#[test]
fn a_completed_checkpoint_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "test-fingerprint").unwrap();
    let output = serde_json::json!({"summary": "hi"});
    ledger.write_completed("summarize", &output).unwrap();
    assert_eq!(ledger.get_completed("summarize").unwrap(), Some(output));
}

#[test]
fn a_skipped_checkpoint_has_no_output_but_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "test-fingerprint").unwrap();
    ledger.write_skipped("handle_pdf").unwrap();
    assert!(ledger.is_skipped("handle_pdf").unwrap());
    assert_eq!(ledger.get_completed("handle_pdf").unwrap(), None);
}

#[test]
fn job_status_starts_running_and_can_be_finalized() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "test-fingerprint").unwrap();
    assert_eq!(
        ledger.job_status().unwrap(),
        cuttlefish_host::ledger::LedgerJobStatus::Running
    );
    ledger.finish("completed").unwrap();
    assert_eq!(
        ledger.job_status().unwrap(),
        cuttlefish_host::ledger::LedgerJobStatus::Completed
    );
}

#[test]
fn reopening_an_existing_ledger_preserves_its_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let ledger = Ledger::open(&path, "test-fingerprint").unwrap();
        ledger.write_completed("a", &serde_json::json!(1)).unwrap();
    }
    let reopened = Ledger::open(&path, "test-fingerprint").unwrap();
    assert_eq!(
        reopened.get_completed("a").unwrap(),
        Some(serde_json::json!(1))
    );
}

#[test]
fn a_real_database_error_is_not_silently_treated_as_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path, "test-fingerprint").unwrap();

    // Drop the checkpoints table out from under the still-open ledger
    // connection via a second connection to the same file. Querying it
    // afterward fails with a real rusqlite error ("no such table"), not
    // `QueryReturnedNoRows` — exactly the distinction get_completed and
    // is_skipped must not collapse into "no checkpoint here".
    let corrupter = rusqlite::Connection::open(&path).unwrap();
    corrupter.execute("DROP TABLE checkpoints", []).unwrap();
    drop(corrupter);

    let completed = ledger.get_completed("some_node");
    assert!(
        completed.is_err(),
        "expected a real database error, got {completed:?}"
    );
    assert!(
        !matches!(completed, Err(rusqlite::Error::QueryReturnedNoRows)),
        "get_completed must not report a genuine error as QueryReturnedNoRows: {completed:?}"
    );

    let skipped = ledger.is_skipped("some_node");
    assert!(
        skipped.is_err(),
        "expected a real database error, got {skipped:?}"
    );
    assert!(
        !matches!(skipped, Err(rusqlite::Error::QueryReturnedNoRows)),
        "is_skipped must not report a genuine error as QueryReturnedNoRows: {skipped:?}"
    );
}

#[test]
fn reopening_with_a_different_fingerprint_keeps_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    Ledger::open(&path, "original").unwrap();
    let reopened = Ledger::open(&path, "different").unwrap();
    assert_eq!(reopened.graph_fingerprint().unwrap(), "original");
}

#[test]
fn jobs_root_honors_cuttlefish_jobs_home_when_set() {
    let _guard = ENV_GUARD.lock().unwrap();
    std::env::set_var("CUTTLEFISH_JOBS_HOME", "/tmp/cf-jobs-test");
    let root = cuttlefish_host::ledger::jobs_root();
    std::env::remove_var("CUTTLEFISH_JOBS_HOME");
    assert_eq!(root, Some(std::path::PathBuf::from("/tmp/cf-jobs-test")));
}

#[test]
fn jobs_root_falls_back_to_cuttlefish_home_jobs_when_unset() {
    let _guard = ENV_GUARD.lock().unwrap();
    std::env::remove_var("CUTTLEFISH_JOBS_HOME");
    std::env::set_var("CUTTLEFISH_HOME", "/tmp/cf-home-test");
    let root = cuttlefish_host::ledger::jobs_root();
    std::env::remove_var("CUTTLEFISH_HOME");
    assert_eq!(
        root,
        Some(std::path::PathBuf::from("/tmp/cf-home-test/jobs"))
    );
}

// --- Per-item fan-out checkpoints ---------------------------------------
//
// A fan-out node runs its block once per manifest item. Each item's outcome
// is checkpointed independently so that (a) a genuinely bad item is recorded
// once and never silently re-run, and (b) work interrupted by a crash is
// picked back up. Those two are distinguished by whether a row exists at all.

#[test]
fn item_checkpoints_are_independent_of_each_other_and_of_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    ledger
        .write_item_completed("map", 0, &serde_json::json!({"a": 1}))
        .unwrap();
    ledger.write_item_failed("map", 1, "boom").unwrap();

    assert_eq!(
        ledger.get_item_completed("map", 0).unwrap(),
        Some(serde_json::json!({"a": 1}))
    );
    assert_eq!(
        ledger.get_item_completed("map", 1).unwrap(),
        None,
        "a failed item has no output"
    );
    assert_eq!(
        ledger.get_item_completed("map", 2).unwrap(),
        None,
        "an item that never ran has no output"
    );
    // The whole-node checkpoint lives at index -1 and must not collide with
    // item 0 -- that collision is exactly what the composite key prevents.
    assert_eq!(ledger.get_completed("map").unwrap(), None);
}

#[test]
fn concluded_items_lists_successes_and_failures_in_index_order() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();
    ledger
        .write_item_completed("map", 2, &serde_json::json!("c"))
        .unwrap();
    ledger
        .write_item_completed("map", 0, &serde_json::json!("a"))
        .unwrap();
    ledger.write_item_failed("map", 1, "bad chunk").unwrap();

    let concluded = ledger.concluded_items("map").unwrap();
    assert_eq!(concluded.len(), 3);
    assert_eq!(concluded[0], (0, Some(serde_json::json!("a")), None));
    assert_eq!(
        concluded[1],
        (1, None, Some("bad chunk".to_string())),
        "written out of order, but must come back in index order"
    );
    assert_eq!(concluded[2], (2, Some(serde_json::json!("c")), None));
}

#[test]
fn a_pre_item_index_ledger_is_refused_with_an_actionable_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    // Hand-build the OLD schema, exactly as a pre-upgrade daemon left it.
    // CREATE TABLE IF NOT EXISTS would otherwise leave this shape in place
    // and fail later at INSERT with "no such column: item_index".
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE checkpoints (
                node_name TEXT PRIMARY KEY, status TEXT NOT NULL,
                output_json TEXT, completed_at TEXT NOT NULL);
             CREATE TABLE job_status (status TEXT NOT NULL, graph_fingerprint TEXT NOT NULL);
             INSERT INTO job_status VALUES ('running', 'fp');",
        )
        .unwrap();
    }
    let err = Ledger::open(&path, "fp").expect_err("an old-schema ledger must be refused");
    let msg = err.to_string();
    assert!(msg.contains("re-submit"), "must say what to do: {msg}");
}

#[test]
fn a_changed_manifest_is_detected_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    // First run records what it fanned out over.
    assert!(ledger
        .check_or_record_manifest("map", "digest-A", 500)
        .unwrap()
        .is_ok());
    // Resuming against the same manifest is fine.
    assert!(ledger
        .check_or_record_manifest("map", "digest-A", 500)
        .unwrap()
        .is_ok());
    // A different manifest means item indices no longer refer to the same
    // inputs -- refuse rather than silently mismatching results to items.
    assert!(ledger
        .check_or_record_manifest("map", "digest-B", 500)
        .unwrap()
        .is_err());
}
