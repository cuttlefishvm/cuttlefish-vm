//! A real fan-out job writing a real warehouse.
//!
//! `warehouse.rs` tests the layers in isolation, from hand-built rows. That
//! cannot catch the thing most likely to be wrong: whether what the *ledger*
//! recorded during an actual run reaches the Parquet files intact — the
//! failed items included, and the lineage populated from the job rather than
//! from a test fixture.

mod support;

use cuttlefish_abi::{JobStatus, Ty};
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::{fanout_collection_ty, CheckedNode};
use cuttlefish_host::infer::StubBackend;
use cuttlefish_host::ledger::Ledger;
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::runner::{run_job, JobSpec, WarehousePlan};
use cuttlefish_host::warehouse::Manifest;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

use arrow_array::{Array, Int64Array, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn interpreter_wasm() -> Vec<u8> {
    static WASM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    WASM.get_or_init(|| {
        let status = support::clean_cargo(env!("CARGO"))
            .args([
                "build",
                "-p",
                "cf-block-rhai-interpreter",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building the rhai interpreter failed");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_rhai_interpreter.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

/// A record type the silver layer can actually type against.
fn item_ty() -> Ty {
    Ty::Record(
        [
            ("name".to_string(), Ty::Text),
            ("length".to_string(), Ty::Number),
            ("ok".to_string(), Ty::Bool),
        ]
        .into_iter()
        .collect(),
    )
}

/// Fan out over the manifest. The item named `bad` fails on purpose — a
/// warehouse that only records successes is one you cannot audit.
fn map_node(manifest: &Path) -> CheckedNode {
    const SCRIPT: &str = r#"
        if input.name == "bad" {
            throw "this item was always going to fail";
        }
        #{ name: input.name, length: input.name.len(), ok: true }
    "#;
    CheckedNode {
        name: "extract".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: Ty::Json,
            output: Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: Some(SCRIPT.to_string()),
        over: Some(manifest.to_path_buf()),
        item_output: Some(item_ty()),
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

/// Reduce to a curated record — this is what gold holds.
fn reduce_node() -> CheckedNode {
    const SCRIPT: &str = r#"
        #{ succeeded: input.succeeded, failed: input.failed }
    "#;
    CheckedNode {
        name: "rollup".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: fanout_collection_ty(),
            output: Ty::Record(
                [
                    ("succeeded".to_string(), Ty::Number),
                    ("failed".to_string(), Ty::Number),
                ]
                .into_iter()
                .collect(),
            ),
        },
        input: Some(cuttlefish_core::graph::InputExpr::FromNode(
            "extract".into(),
        )),
        repeat_until: None,
        max_iterations: None,
        script: Some(SCRIPT.to_string()),
        over: None,
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

fn read_table(path: &Path) -> Vec<arrow_array::RecordBatch> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("readable Parquet")
        .build()
        .unwrap()
        .map(|b| b.unwrap())
        .collect()
}

fn strings<'a>(batch: &'a arrow_array::RecordBatch, name: &str) -> &'a StringArray {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column `{name}` must exist"))
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| panic!("column `{name}` must be a string column"))
}

#[tokio::test]
async fn a_real_fanout_job_writes_a_warehouse_a_foreign_reader_can_open() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.jsonl");
    std::fs::write(
        &manifest_path,
        "{\"name\": \"alpha\"}\n{\"name\": \"bad\"}\n{\"name\": \"charlie\"}\n",
    )
    .unwrap();

    let root = dir.path().join("warehouse");
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fingerprint-1").unwrap();
    let (tx, _rx) = mpsc::channel(1024);

    let job = JobSpec {
        nodes: vec![map_node(&manifest_path), reduce_node()],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: Some(WarehousePlan {
            root: root.clone(),
            required: false,
            spec_name: "index_corpus".into(),
            model: "stub".into(),
            embedding_model: None,
        }),
    };

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");

    let manifest: Manifest =
        serde_json::from_str(&std::fs::read_to_string(root.join("manifest.json")).unwrap())
            .expect("the manifest parses");
    assert_eq!(manifest.spec_fingerprint, "fingerprint-1");

    // --- bronze: every item, the failure included -----------------------
    let bronze = read_table(&root.join("bronze/extract.parquet"));
    let total: usize = bronze.iter().map(|b| b.num_rows()).sum();
    assert_eq!(
        total, 3,
        "bronze holds all three items, not just the two that worked"
    );

    let b = &bronze[0];
    let statuses = strings(b, "status");
    let errors = strings(b, "error");
    let failed: Vec<_> = (0..b.num_rows())
        .filter(|&i| statuses.value(i) != "completed")
        .collect();
    assert_eq!(failed.len(), 1, "exactly one item failed");
    assert!(
        errors.value(failed[0]).contains("always going to fail"),
        "the failure's own message survives into bronze: {}",
        errors.value(failed[0])
    );

    // Lineage comes from the run, not from a fixture.
    assert_eq!(strings(b, "spec_name").value(0), "index_corpus");
    assert_eq!(strings(b, "spec_fingerprint").value(0), "fingerprint-1");
    assert_eq!(
        strings(b, "cuttlefish_version").value(0),
        env!("CARGO_PKG_VERSION")
    );
    // The item's own input, recorded by the ledger during the run.
    assert!(
        strings(b, "source_input").value(0).contains("alpha"),
        "provenance names the item that produced the row: {}",
        strings(b, "source_input").value(0)
    );

    // --- silver: successes only, and genuinely typed ---------------------
    let silver = read_table(&root.join("silver/extract.parquet"));
    let total: usize = silver.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2, "the failed item does not reach silver");

    let s = &silver[0];
    let lengths = s
        .column_by_name("f_length")
        .expect("the declared number field is a column")
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("a declared `number` is an integer column, not a stringified one");
    // "alpha" and "charlie" — the values the block actually computed.
    let mut got: Vec<i64> = (0..s.num_rows()).map(|i| lengths.value(i)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![5, 7]);

    let ok = s
        .column_by_name("f_ok")
        .expect("the declared bool field is a column")
        .as_any()
        .downcast_ref::<arrow_array::BooleanArray>()
        .expect("a declared `bool` is a boolean column");
    assert!(ok.value(0));

    // --- gold: the rollup's own curated output ---------------------------
    let gold = read_table(&root.join("gold/rollup.parquet"));
    let total: usize = gold.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 1, "gold is the one curated record");
    let g = &gold[0];
    let succeeded = g
        .column_by_name("f_succeeded")
        .unwrap()
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let failed_count = g
        .column_by_name("f_failed")
        .unwrap()
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!((succeeded.value(0), failed_count.value(0)), (2, 1));
}

#[tokio::test]
async fn a_job_that_declared_no_warehouse_writes_none() {
    // Opt-in means opt-in: a spec that never asked must not find files it
    // did not ask for next to its results.
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.jsonl");
    std::fs::write(&manifest_path, "{\"name\": \"alpha\"}\n").unwrap();

    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();
    let (tx, _rx) = mpsc::channel(1024);
    let job = JobSpec {
        nodes: vec![map_node(&manifest_path), reduce_node()],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");

    let stray: Vec<_> = walkdir(dir.path())
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "parquet"))
        .collect();
    assert!(stray.is_empty(), "no warehouse was asked for: {stray:?}");
}

fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn a_resumed_job_writes_a_whole_warehouse_not_a_partial_one() {
    // The claim this checks: because the warehouse is written once at the end
    // from the ledger, a run that was interrupted and then resumed produces a
    // warehouse describing *every* item — the ones the first attempt already
    // concluded included — rather than only what the resuming attempt itself
    // ran. Writing per node as each concluded would fail this.
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.jsonl");
    std::fs::write(
        &manifest_path,
        "{\"name\": \"alpha\"}\n{\"name\": \"bad\"}\n{\"name\": \"charlie\"}\n",
    )
    .unwrap();

    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(std::fs::read(&manifest_path).unwrap());
        cuttlefish_host::hex::encode(h.finalize())
    };

    let root = dir.path().join("warehouse");
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fingerprint-1").unwrap();
    ledger
        .check_or_record_manifest("extract", &digest, 3)
        .unwrap()
        .unwrap();

    // Exactly what an interrupted first attempt would have left: item 0
    // concluded, items 1 and 2 never reached. The sentinel `length` could not
    // be produced by the script, so a re-run would overwrite it.
    ledger
        .write_item_completed(
            "extract",
            0,
            &serde_json::json!({"name": "alpha.txt", "length": 999, "ok": true}),
            Some(&serde_json::json!({"name": "alpha", "from": "the interrupted attempt"})),
        )
        .unwrap();

    let (tx, _rx) = mpsc::channel(1024);
    let job = JobSpec {
        nodes: vec![map_node(&manifest_path), reduce_node()],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: Some(WarehousePlan {
            root: root.clone(),
            required: false,
            spec_name: "index_corpus".into(),
            model: "stub".into(),
            embedding_model: None,
        }),
    };
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");

    let bronze = read_table(&root.join("bronze/extract.parquet"));
    let total: usize = bronze.iter().map(|b| b.num_rows()).sum();
    assert_eq!(
        total, 3,
        "the warehouse covers every item, not only those this attempt ran"
    );

    // The pre-seeded item's own recorded values survive, which is what shows
    // the warehouse was built from the ledger rather than from this run.
    let silver = read_table(&root.join("silver/extract.parquet"));
    let s = &silver[0];
    let lengths = s
        .column_by_name("f_length")
        .unwrap()
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let all: Vec<i64> = (0..s.num_rows()).map(|i| lengths.value(i)).collect();
    assert!(
        all.contains(&999),
        "the already-concluded item's recorded output reaches the warehouse: {all:?}"
    );

    // And its provenance is the one recorded then, not one reconstructed now.
    let b = &bronze[0];
    let sources = strings(b, "source_input");
    assert!(
        (0..b.num_rows()).any(|i| sources.value(i).contains("the interrupted attempt")),
        "lineage comes from the ledger, so it survives the interruption"
    );
}

#[tokio::test]
async fn required_warehouse_failure_is_terminal_and_resume_keeps_completed_items() {
    strict_or_optional_warehouse(true).await;
}

#[tokio::test]
async fn optional_warehouse_retains_warning_only_failure_behavior() {
    strict_or_optional_warehouse(false).await;
}

async fn strict_or_optional_warehouse(required: bool) {
    use cuttlefish_host::infer::{InferBackend, InferRequest, InferResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counted(AtomicUsize);
    #[async_trait::async_trait]
    impl InferBackend for Counted {
        async fn infer(
            &self,
            req: InferRequest<'_>,
            on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
        ) -> anyhow::Result<InferResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            StubBackend::default().infer(req, on_token).await
        }
        fn model_name(&self) -> String {
            "stub".into()
        }
    }
    let backend = Arc::new(Counted(AtomicUsize::new(0)));
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("input.jsonl");
    std::fs::write(&manifest, "{\"name\":\"alpha\"}\n").unwrap();
    let root = dir.path().join("warehouse");
    // A file cannot hold the warehouse's subdirectories.
    std::fs::write(&root, "occupied").unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "strict-1").unwrap();
    let make_job = || JobSpec {
        nodes: vec![
            {
                let mut node = map_node(&manifest);
                node.script = Some(format!(
                    "let response = infer(\"counted fixture\", 8); {}",
                    node.script.as_ref().unwrap()
                ));
                node
            },
            reduce_node(),
        ],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: Some(WarehousePlan {
            root: root.clone(),
            required,
            spec_name: "strict".into(),
            model: "stub".into(),
            embedding_model: None,
        }),
    };
    let (tx, _rx) = mpsc::channel(1024);
    let engine = Arc::new(Engine::default());
    let cache = ModuleCache::new();
    let failed = run_job(
        engine.clone(),
        backend.clone(),
        make_job(),
        tx.clone(),
        CancellationToken::new(),
        &ledger,
        &cache,
    )
    .await;
    if required {
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.error.unwrap().code, "warehouse_publication_failed");
    } else {
        assert_eq!(failed.status, JobStatus::Completed);
        assert!(failed.error.is_none());
        return;
    }
    assert_eq!(backend.0.load(Ordering::SeqCst), 1);
    let before = ledger.concluded_rows("extract").unwrap();
    assert_eq!(before.len(), 1, "expensive completed work is retained");
    std::fs::remove_file(&root).unwrap();
    let completed = run_job(
        engine,
        backend.clone(),
        make_job(),
        tx,
        CancellationToken::new(),
        &ledger,
        &cache,
    )
    .await;
    assert_eq!(completed.status, JobStatus::Completed, "{completed:?}");
    assert_eq!(
        backend.0.load(Ordering::SeqCst),
        1,
        "resume must not call inference again"
    );
    let after = ledger.concluded_rows("extract").unwrap();
    assert_eq!(
        before[0].concluded_at, after[0].concluded_at,
        "resume must reuse the item"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("publication.json")).unwrap()).unwrap();
    assert_eq!(receipt["spec_fingerprint"], "strict-1");
    assert!(receipt["tables"].as_object().unwrap().contains_key(
        &Path::new("bronze")
            .join("extract.parquet")
            .to_string_lossy()
            .into_owned()
    ));
}

#[tokio::test]
async fn synthetic_candidate_example_preserves_grouping_and_rejects_heldout() {
    let dir = tempfile::tempdir().unwrap();
    let packet: serde_json::Value = serde_json::from_str(include_str!(
        "../../../examples/synthetic-candidates/fixture.jsonl"
    ))
    .unwrap();
    for heldout in [false, true] {
        let mut packet = packet.clone();
        if heldout {
            packet["split"] = "heldout".into();
        }
        let manifest = dir.path().join(if heldout {
            "heldout.jsonl"
        } else {
            "train.jsonl"
        });
        std::fs::write(&manifest, format!("{packet}\n")).unwrap();
        let mut candidate = map_node(&manifest);
        candidate.script =
            Some(include_str!("../../../examples/synthetic-candidates/candidate.rhai").into());
        let declared: cuttlefish_abi::Signature = candidate
            .script
            .as_ref()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .strip_prefix("//! signature: ")
            .unwrap()
            .parse()
            .unwrap();
        candidate.item_output = Some(declared.output);
        let mut complete = reduce_node();
        complete.script =
            Some(include_str!("../../../examples/synthetic-candidates/completion.rhai").into());
        complete.signature = complete
            .script
            .as_ref()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .strip_prefix("//! signature: ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(complete.signature.input, fanout_collection_ty());
        let ledger = Ledger::open(
            &dir.path().join(if heldout {
                "heldout.sqlite"
            } else {
                "train.sqlite"
            }),
            "candidate-test",
        )
        .unwrap();
        let (tx, _) = mpsc::channel(1024);
        let result = run_job(Arc::new(Engine::default()), Arc::new(StubBackend {
            reply: "{\"question\":\"Which current documents support the required therapy duration?\",\"reference_evidence\":[\"wrong-patient\"]}".into()
        }), JobSpec {
            nodes: vec![candidate,complete], exclusive_to: HashMap::new(), input: serde_json::Value::Null,
            caps: Capabilities::new(vec![dir.path().to_path_buf()]), alternates: Default::default(), embedder: None, warehouse: None,
        },tx,CancellationToken::new(),&ledger,&ModuleCache::new()).await;
        if heldout {
            assert_eq!(result.status, JobStatus::Failed, "{result:?}");
        } else {
            assert_eq!(result.status, JobStatus::Completed, "{result:?}");
            let rows = ledger.concluded_rows("extract").unwrap();
            let output = rows[0].output.as_ref().unwrap();
            assert_eq!(output["parent_id"], packet["parent_id"]);
            assert_eq!(output["book_id"], packet["book_id"]);
            assert_eq!(output["documents"], packet["documents"]);
            assert_eq!(output["patient"], packet["patient"]);
            assert_eq!(output["required_weeks"], packet["required_weeks"]);
            assert_eq!(output["reference_evidence"], serde_json::json!(["current"]));
            assert_eq!(output["requires_review"], true);
        }
    }
}
