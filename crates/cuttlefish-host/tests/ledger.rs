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
    assert_eq!(ledger.job_status().unwrap(), cuttlefish_host::ledger::LedgerJobStatus::Running);
    ledger.finish("completed").unwrap();
    assert_eq!(ledger.job_status().unwrap(), cuttlefish_host::ledger::LedgerJobStatus::Completed);
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
    assert_eq!(reopened.get_completed("a").unwrap(), Some(serde_json::json!(1)));
}
