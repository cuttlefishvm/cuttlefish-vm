//! Reading the warehouse back.
//!
//! Building a `RecordBatch` in memory proves the columns line up; it does not
//! prove anybody else can open the file. These tests write real Parquet to
//! disk and read it with the reader, because the whole reason for this output
//! format is that a tool cuttlefish does not control opens it.

use arrow_array::{Array, Int64Array, StringArray};
use cuttlefish_abi::Ty;
use cuttlefish_host::warehouse::{
    bronze_batch, entry_for, silver_batch, write_manifest, write_parquet, Layer, Lineage, Manifest,
    Row,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn lineage() -> Lineage {
    Lineage {
        job_id: "job-1".into(),
        spec_name: "index_corpus".into(),
        spec_fingerprint: "abc123".into(),
        model: "ollama:llama3.2:1b".into(),
        embedding_model: Some("ollama:nomic-embed-text".into()),
        cuttlefish_version: "0.8.0".into(),
    }
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            node: "extract".into(),
            item: 0,
            status: "completed".into(),
            concluded_at: "2026-08-18T00:00:00Z".into(),
            source_input: Some(r#"{"path":"a.pdf"}"#.into()),
            output: Some(
                serde_json::json!({"title": "Annual Report", "pages": 227, "has_text": true}),
            ),
            error: None,
        },
        Row {
            node: "extract".into(),
            item: 1,
            status: "failed".into(),
            concluded_at: "2026-08-18T00:00:01Z".into(),
            source_input: Some(r#"{"path":"b.pdf"}"#.into()),
            output: None,
            error: Some("pdf has no text layer".into()),
        },
    ]
}

/// Read every batch out of a Parquet file.
fn read_back(path: &std::path::Path) -> Vec<arrow_array::RecordBatch> {
    let file = std::fs::File::open(path).expect("the file must exist");
    ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("the file must be readable Parquet")
        .build()
        .expect("building the reader")
        .map(|b| b.expect("reading a batch"))
        .collect()
}

#[test]
fn bronze_round_trips_through_a_real_parquet_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bronze/extract.parquet");

    let batch = bronze_batch(&rows(), &lineage()).unwrap();
    write_parquet(&path, &batch).unwrap();

    let read = read_back(&path);
    let total: usize = read.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2, "both the success and the failure survive");

    let b = &read[0];
    let errors = b
        .column_by_name("error")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("`error` reads back as a string column");
    assert!(errors.is_null(0), "the successful item has no error");
    assert_eq!(
        errors.value(1),
        "pdf has no text layer",
        "the failure's reason survives the round trip verbatim"
    );

    let items = b
        .column_by_name("item")
        .unwrap()
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("`item` reads back as an int column, not a stringified one");
    assert_eq!((items.value(0), items.value(1)), (0, 1));
}

#[test]
fn a_silver_row_reads_back_as_the_value_not_its_json_encoding() {
    // The failure this guards is invisible in a schema dump: every cell
    // reading `"Annual Report"`, quotes included, which breaks every join
    // and every equality filter downstream.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silver/extract.parquet");

    let ty = Ty::Record(
        [
            ("title".to_string(), Ty::Text),
            ("pages".to_string(), Ty::Number),
            ("has_text".to_string(), Ty::Bool),
        ]
        .into_iter()
        .collect(),
    );

    let batch = silver_batch(&rows(), &lineage(), &ty).unwrap().unwrap();
    write_parquet(&path, &batch).unwrap();

    let read = read_back(&path);
    let total: usize = read.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 1, "only the successful item reaches silver");

    let titles = read[0]
        .column_by_name("f_title")
        .expect("the declared field is a column")
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(titles.value(0), "Annual Report");

    // Lineage survives into silver too — that is what makes the layer
    // traceable on its own rather than only via a join back to bronze.
    let jobs = read[0]
        .column_by_name("job_id")
        .expect("silver carries lineage")
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(jobs.value(0), "job-1");
}

#[test]
fn a_manifest_records_a_skipped_layer_with_its_reason() {
    // A reader finding no silver table has to be able to tell "the author
    // declared no shape" from "the job died before writing it".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bronze/extract.parquet");
    let batch = bronze_batch(&rows(), &lineage()).unwrap();
    write_parquet(&path, &batch).unwrap();

    let manifest = Manifest {
        job_id: "job-1".into(),
        spec_name: "index_corpus".into(),
        spec_fingerprint: "abc123".into(),
        model: "ollama:llama3.2:1b".into(),
        embedding_model: None,
        cuttlefish_version: "0.8.0".into(),
        written_at: "2026-08-18T00:00:02Z".into(),
        bronze: [(
            "extract".to_string(),
            Layer::Written(entry_for(dir.path(), &path, &batch)),
        )]
        .into_iter()
        .collect(),
        silver: [(
            "extract".to_string(),
            Layer::Skipped {
                skipped: "node `extract` declares a Json output; there is no shape to validate"
                    .into(),
            },
        )]
        .into_iter()
        .collect(),
        gold: Default::default(),
    };

    let written = write_manifest(dir.path(), &manifest).unwrap();
    let text = std::fs::read_to_string(&written).unwrap();
    let parsed: Manifest = serde_json::from_str(&text).expect("the manifest round trips");

    match parsed.silver.get("extract").expect("silver is recorded") {
        Layer::Skipped { skipped } => assert!(skipped.contains("Json"), "{skipped}"),
        Layer::Written(_) => panic!("this layer was skipped, not written"),
    }
    match parsed.bronze.get("extract").expect("bronze is recorded") {
        Layer::Written(entry) => {
            assert_eq!(entry.rows, 2);
            // Relative, so copying the warehouse elsewhere keeps it valid.
            assert_eq!(entry.path, "bronze/extract.parquet");
            assert!(!entry.path.starts_with('/'), "{}", entry.path);
        }
        Layer::Skipped { .. } => panic!("bronze was written"),
    }
}

/// Emit a warehouse to `CUTTLEFISH_WAREHOUSE_OUT` when set.
///
/// Not an assertion — a fixture generator, so a foreign reader (pyarrow) can
/// be pointed at real output. Reading the file with the same library that
/// wrote it cannot show that a *different* implementation agrees, and "another
/// tool can open this" is the entire reason for the format.
#[test]
fn emit_a_fixture_warehouse_when_asked() {
    let Ok(out) = std::env::var("CUTTLEFISH_WAREHOUSE_OUT") else {
        return;
    };
    let root = std::path::PathBuf::from(out);
    let ty = Ty::Record(
        [
            ("title".to_string(), Ty::Text),
            ("pages".to_string(), Ty::Number),
            ("has_text".to_string(), Ty::Bool),
        ]
        .into_iter()
        .collect(),
    );

    let bronze = bronze_batch(&rows(), &lineage()).unwrap();
    let bronze_path = root.join("bronze/extract.parquet");
    write_parquet(&bronze_path, &bronze).unwrap();

    let silver = silver_batch(&rows(), &lineage(), &ty).unwrap().unwrap();
    let silver_path = root.join("silver/extract.parquet");
    write_parquet(&silver_path, &silver).unwrap();

    let manifest = Manifest {
        job_id: "job-1".into(),
        spec_name: "index_corpus".into(),
        spec_fingerprint: "abc123".into(),
        model: "ollama:llama3.2:1b".into(),
        embedding_model: Some("ollama:nomic-embed-text".into()),
        cuttlefish_version: "0.8.0".into(),
        written_at: "2026-08-18T00:00:02Z".into(),
        bronze: [(
            "extract".to_string(),
            Layer::Written(entry_for(&root, &bronze_path, &bronze)),
        )]
        .into_iter()
        .collect(),
        silver: [(
            "extract".to_string(),
            Layer::Written(entry_for(&root, &silver_path, &silver)),
        )]
        .into_iter()
        .collect(),
        gold: Default::default(),
    };
    write_manifest(&root, &manifest).unwrap();
}
