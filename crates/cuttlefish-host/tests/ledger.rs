use cuttlefish_host::ledger::Ledger;

#[test]
fn a_fresh_ledger_has_no_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
    assert_eq!(ledger.get_completed("some_node").unwrap(), None);
}

#[test]
fn a_completed_checkpoint_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
    let output = serde_json::json!({"summary": "hi"});
    ledger.write_completed("summarize", &output).unwrap();
    assert_eq!(ledger.get_completed("summarize").unwrap(), Some(output));
}

#[test]
fn a_skipped_checkpoint_has_no_output_but_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
    ledger.write_skipped("handle_pdf").unwrap();
    assert!(ledger.is_skipped("handle_pdf").unwrap());
    assert_eq!(ledger.get_completed("handle_pdf").unwrap(), None);
}

#[test]
fn job_status_starts_running_and_can_be_finalized() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
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
        let ledger = Ledger::open(&path).unwrap();
        ledger.write_completed("a", &serde_json::json!(1)).unwrap();
    }
    let reopened = Ledger::open(&path).unwrap();
    assert_eq!(
        reopened.get_completed("a").unwrap(),
        Some(serde_json::json!(1))
    );
}

#[test]
fn a_real_database_error_is_not_silently_treated_as_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();

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
