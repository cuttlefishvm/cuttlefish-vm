//! Writing a job's results as a medallion warehouse.
//!
//! A pipeline that produces JSONL produces something a person can read and
//! nothing a query engine can. The warehouse is the same rows in Parquet,
//! laid out the way data engineering already lays this out:
//!
//! - **bronze** — every concluded item, success *and* failure, exactly as the
//!   ledger recorded it. Append-only and lossy about nothing. The failures
//!   belong here: a bronze layer that silently drops what went wrong is a
//!   bronze layer you cannot audit, and "which items failed and why" is a
//!   question people ask of the warehouse, not of the log.
//! - **silver** — the successful rows, *typed* against the output type the
//!   node declared. Validation is the point of the layer, so a node that
//!   declares [`Ty::Json`] has nothing to validate against and gets no silver
//!   table. That is recorded in the manifest with the reason, rather than
//!   emitting one JSON-blob column and calling it typed.
//! - **gold** — the rollup node's own output: curated, aggregate, and by
//!   nature defined by whoever wrote the spec rather than by cuttlefish.
//!
//! Every bronze and silver row carries its own lineage columns. That
//! duplicates data, deliberately: a Parquet file gets copied, attached, and
//! handed to somebody who does not have the manifest, and a row that cannot
//! answer "where did you come from" once separated from its manifest is a row
//! whose provenance depends on filesystem luck.
//!
//! # On the source column
//!
//! Lineage records the item's input *verbatim*, as JSON, in `source_input`.
//! It would read better to publish a `source_uri` — but which key of the input
//! holds the path is the spec author's business, not cuttlefish's, and a guess
//! ("try `path`, then `url`, then `file`") produces a column that is right for
//! the corpora we happened to test and silently empty for everyone else. The
//! verbatim input is always correct and always complete.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{
    ArrayBuilder, BooleanBuilder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use cuttlefish_abi::Ty;

/// What went wrong writing a warehouse.
#[derive(Debug, thiserror::Error)]
pub enum WarehouseError {
    /// A directory or file could not be created.
    #[error("creating {path}: {source}")]
    Create {
        /// What could not be created.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Parquet encoding failed.
    #[error("writing {path}: {source}")]
    Write {
        /// The table being written.
        path: PathBuf,
        /// The underlying Parquet error.
        #[source]
        source: parquet::errors::ParquetError,
    },
    /// Columns and rows did not line up — a bug here, not bad input.
    #[error("building a record batch for {table}: {source}")]
    Batch {
        /// Which layer was being assembled.
        table: String,
        /// The underlying Arrow error.
        #[source]
        source: arrow_schema::ArrowError,
    },
    /// The manifest could not be encoded as JSON.
    #[error("serializing the manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    /// The manifest encoded but could not be written.
    #[error("writing the manifest to {path}: {source}")]
    ManifestWrite {
        /// Where the manifest was to go.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Lineage carried by every bronze and silver row.
///
/// Job-level rather than row-level values are still written per row — see the
/// module docs on why the duplication is deliberate.
#[derive(Debug, Clone)]
pub struct Lineage {
    /// The job directory's name, which is the job id.
    pub job_id: String,
    /// The spec this job ran.
    pub spec_name: String,
    /// The graph fingerprint the ledger recorded, which pins *which* pipeline
    /// produced these rows. Two runs of "the same" spec with an edited block
    /// have different fingerprints, and that difference is the whole reason
    /// somebody re-reads their warehouse six months later.
    pub spec_fingerprint: String,
    /// The chat model, as resolved.
    pub model: String,
    /// The embedding model, when the spec declared one.
    pub embedding_model: Option<String>,
    /// The cuttlefish that wrote this. A column, not just a manifest field,
    /// because the row format is this version's and a reader deserves to know
    /// which version's rules it is reading under.
    pub cuttlefish_version: String,
}

/// The lineage columns, in the order they are written.
///
/// Shared by bronze and silver so the two layers are joinable on identical
/// column names and types rather than nearly-identical ones.
fn lineage_fields() -> Vec<Field> {
    vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("node", DataType::Utf8, false),
        Field::new("item", DataType::Int64, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("concluded_at", DataType::Utf8, false),
        Field::new("source_input", DataType::Utf8, true),
        Field::new("spec_name", DataType::Utf8, false),
        Field::new("spec_fingerprint", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, false),
        Field::new("embedding_model", DataType::Utf8, true),
        Field::new("cuttlefish_version", DataType::Utf8, false),
    ]
}

/// One concluded item, as the warehouse sees it.
#[derive(Debug, Clone)]
pub struct Row {
    /// The fan-out node that produced this item.
    pub node: String,
    /// The item's index in its manifest — the key that ties a warehouse row
    /// back to `results.jsonl` and to `cuttlefish escalations`.
    pub item: i64,
    /// `completed`, `failed`, or `escalated`, verbatim. Kept distinct because
    /// `escalated` means a human was asked and `failed` means it simply did
    /// not work.
    pub status: String,
    /// When the item concluded, RFC 3339.
    pub concluded_at: String,
    /// The item's input, verbatim JSON. `None` for a ledger predating the
    /// column — absent rather than invented.
    pub source_input: Option<String>,
    /// The item's output. `None` for a failure.
    pub output: Option<serde_json::Value>,
    /// Why it failed. `None` for a success.
    pub error: Option<String>,
}

/// Whether a declared [`Ty`] can be a flat silver column at all.
///
/// A nested record, a list, or a handle has no sensible single column. Those
/// stay in bronze as JSON rather than being flattened into names like
/// `field__sub__leaf`, which read as schema but are really string
/// concatenation.
fn is_flattenable(ty: &Ty) -> bool {
    match ty {
        Ty::Text | Ty::Number | Ty::Bool => true,
        // A *leaf* the author named but whose shape they left open. Kept as
        // its JSON text. Different from a whole node declaring `Json`, which
        // gets no silver table at all: there, nothing was named.
        Ty::Json => true,
        Ty::Bytes | Ty::Image | Ty::Document => false,
        Ty::List(_) | Ty::Record(_) => false,
    }
}

/// The Arrow type for a declared field, given the values actually present.
///
/// Only [`Ty::Number`] consults the values, and only to choose between
/// `Int64` and `Float64`: JSON has one number type, so the distinction exists
/// nowhere in the declaration and can only come from the data. Every value
/// integral means an integer column — the difference between a page count
/// reading `227` and `227.0`, and between an exact join key and a float one.
fn column_type(name: &str, ty: &Ty, rows: &[Row]) -> DataType {
    match ty {
        Ty::Bool => DataType::Boolean,
        Ty::Number => {
            let fractional = rows.iter().filter_map(|r| r.output.as_ref()).any(|out| {
                out.get(name)
                    .and_then(|v| v.as_f64())
                    .is_some_and(|f| f.fract() != 0.0)
            });
            if fractional {
                DataType::Float64
            } else {
                // Also the empty and all-null case. An integer column that
                // turns out to hold no values is harmless; guessing float
                // would make every downstream key a float forever.
                DataType::Int64
            }
        }
        _ => DataType::Utf8,
    }
}

/// The declared fields that become silver columns, in schema order.
fn silver_columns(fields: &BTreeMap<String, Ty>) -> Vec<(&String, &Ty)> {
    fields.iter().filter(|(_, ty)| is_flattenable(ty)).collect()
}

/// The columns a node's declared output type contributes to silver.
///
/// `None` when the node declared no shape with named, flattenable fields —
/// [`Ty::Json`] most of all. Silver means "validated against a declared
/// shape", and there is no shape to validate against, so the honest answer is
/// no table rather than one JSON-blob column called typed.
///
/// Takes the rows because [`Ty::Number`] cannot pick between an integer and a
/// float column without them: every value integral means an integer column.
pub fn silver_schema(item_output: &Ty, rows: &[Row]) -> Option<Schema> {
    let Ty::Record(fields) = item_output else {
        return None;
    };
    let declared = silver_columns(fields);
    if declared.is_empty() {
        return None;
    }

    let mut out = lineage_fields();
    for (name, ty) in declared {
        // Nullable throughout: a block may legitimately omit an optional
        // field, and a non-null column would turn that into a write failure
        // at the end of a long job rather than a null in a cell.
        //
        // Prefixed `f_` so a block naming a field `model` or `item` cannot
        // collide with a lineage column.
        out.push(Field::new(
            format!("f_{name}"),
            column_type(name, ty, rows),
            true,
        ));
    }
    Some(Schema::new(out))
}

/// The bronze schema: lineage, plus the raw output and error.
pub fn bronze_schema() -> Schema {
    let mut fields = lineage_fields();
    fields.push(Field::new("output_json", DataType::Utf8, true));
    fields.push(Field::new("error", DataType::Utf8, true));
    Schema::new(fields)
}

/// Fill the lineage columns for one row into the builders that hold them.
fn push_lineage(builders: &mut [Box<dyn ArrayBuilder>], row: &Row, lineage: &Lineage) {
    macro_rules! s {
        ($i:expr, $v:expr) => {
            builders[$i]
                .as_any_mut()
                .downcast_mut::<StringBuilder>()
                .expect("lineage column is a string column")
                .append_option($v)
        };
    }
    s!(0, Some(&lineage.job_id));
    s!(1, Some(&row.node));
    builders[2]
        .as_any_mut()
        .downcast_mut::<Int64Builder>()
        .expect("`item` is an int column")
        .append_value(row.item);
    s!(3, Some(&row.status));
    s!(4, Some(&row.concluded_at));
    s!(5, row.source_input.as_ref());
    s!(6, Some(&lineage.spec_name));
    s!(7, Some(&lineage.spec_fingerprint));
    s!(8, Some(&lineage.model));
    s!(9, lineage.embedding_model.as_ref());
    s!(10, Some(&lineage.cuttlefish_version));
}

/// Fresh builders matching `schema`, in order.
fn builders_for(schema: &Schema) -> Vec<Box<dyn ArrayBuilder>> {
    schema
        .fields()
        .iter()
        .map(|f| -> Box<dyn ArrayBuilder> {
            match f.data_type() {
                DataType::Int64 => Box::new(Int64Builder::new()),
                DataType::Float64 => Box::new(Float64Builder::new()),
                DataType::Boolean => Box::new(BooleanBuilder::new()),
                _ => Box::new(StringBuilder::new()),
            }
        })
        .collect()
}

/// A JSON value as the text a `Utf8` column should hold.
///
/// A JSON string becomes its contents, not a quoted re-encoding: a text field
/// whose cells all read `"hello"` with the quotes is the classic sign of a
/// pipeline that serialized one layer too many.
fn cell_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Seal the builders into arrays, in column order.
fn finish(mut builders: Vec<Box<dyn ArrayBuilder>>) -> Vec<ArrayRef> {
    builders.iter_mut().map(|b| b.finish()).collect()
}

/// Build the bronze batch: every row, success and failure alike.
pub fn bronze_batch(rows: &[Row], lineage: &Lineage) -> Result<RecordBatch, WarehouseError> {
    let schema = bronze_schema();
    let mut builders = builders_for(&schema);
    let lineage_count = lineage_fields().len();

    for row in rows {
        push_lineage(&mut builders, row, lineage);
        let output = row.output.as_ref().and_then(cell_text);
        builders[lineage_count]
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .expect("`output_json` is a string column")
            .append_option(output);
        builders[lineage_count + 1]
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .expect("`error` is a string column")
            .append_option(row.error.as_ref());
    }

    RecordBatch::try_new(Arc::new(schema), finish(builders)).map_err(|e| WarehouseError::Batch {
        table: "bronze".into(),
        source: e,
    })
}

/// Build the silver batch: successful rows only, typed against `item_output`.
///
/// Returns `None` when the node declared no shape to validate against.
pub fn silver_batch(
    rows: &[Row],
    lineage: &Lineage,
    item_output: &Ty,
) -> Result<Option<RecordBatch>, WarehouseError> {
    let Some(schema) = silver_schema(item_output, rows) else {
        return Ok(None);
    };
    let Ty::Record(fields) = item_output else {
        return Ok(None);
    };
    let declared = silver_columns(fields);

    let mut builders = builders_for(&schema);
    let lineage_count = lineage_fields().len();

    for row in rows {
        // Failures carry no output to type. They are already in bronze, which
        // is where somebody auditing goes; silver is the layer people join
        // against, and a half-populated row in it is worse than no row.
        let Some(output) = &row.output else { continue };
        push_lineage(&mut builders, row, lineage);

        for (offset, (name, _)) in declared.iter().enumerate() {
            let column = lineage_count + offset;
            let value = output.get(name.as_str());
            let builder = &mut builders[column];
            match schema.field(column).data_type() {
                DataType::Int64 => builder
                    .as_any_mut()
                    .downcast_mut::<Int64Builder>()
                    .expect("an Int64 column has an Int64 builder")
                    .append_option(value.and_then(|v| v.as_i64())),
                DataType::Float64 => builder
                    .as_any_mut()
                    .downcast_mut::<Float64Builder>()
                    .expect("a Float64 column has a Float64 builder")
                    .append_option(value.and_then(|v| v.as_f64())),
                DataType::Boolean => builder
                    .as_any_mut()
                    .downcast_mut::<BooleanBuilder>()
                    .expect("a Boolean column has a Boolean builder")
                    .append_option(value.and_then(|v| v.as_bool())),
                _ => builder
                    .as_any_mut()
                    .downcast_mut::<StringBuilder>()
                    .expect("every other column is a string column")
                    .append_option(value.and_then(cell_text)),
            }
        }
    }

    let arrays = finish(builders);
    RecordBatch::try_new(Arc::new(schema), arrays)
        .map(Some)
        .map_err(|e| WarehouseError::Batch {
            table: "silver".into(),
            source: e,
        })
}

/// Write one batch to `path` as Parquet.
pub fn write_parquet(path: &Path, batch: &RecordBatch) -> Result<(), WarehouseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| WarehouseError::Create {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = std::fs::File::create(path).map_err(|e| WarehouseError::Create {
        path: path.to_path_buf(),
        source: e,
    })?;

    let props = parquet::file::properties::WriterProperties::builder()
        // Snappy rather than zstd: every reader that will open these files
        // has supported snappy since Parquet existed, and the point of this
        // output is that somebody else's tool can read it.
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();

    let mut writer = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), Some(props))
        .map_err(|e| WarehouseError::Write {
            path: path.to_path_buf(),
            source: e,
        })?;
    writer.write(batch).map_err(|e| WarehouseError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;
    writer.close().map_err(|e| WarehouseError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// What a manifest says about one table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableEntry {
    /// Path relative to the warehouse root, so a warehouse stays valid when
    /// moved or copied somewhere else.
    pub path: String,
    /// How many rows the table holds.
    pub rows: usize,
    /// The column names, in schema order.
    pub columns: Vec<String>,
}

/// A layer either has a table or has a reason it doesn't.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Layer {
    /// The table exists; here is where and how big.
    Written(TableEntry),
    /// Recorded rather than omitted: a reader who finds no silver table needs
    /// to know whether the layer was skipped or the job broke.
    Skipped {
        /// Why there is no table, in words a spec author can act on.
        skipped: String,
    },
}

/// The manifest written at the warehouse root.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The job that produced this warehouse.
    pub job_id: String,
    /// The spec it ran.
    pub spec_name: String,
    /// Which pipeline, exactly — see [`Lineage::spec_fingerprint`].
    pub spec_fingerprint: String,
    /// The chat model, as resolved.
    pub model: String,
    /// The embedding model, if the spec declared one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// The cuttlefish that wrote this.
    pub cuttlefish_version: String,
    /// When it was written, RFC 3339.
    pub written_at: String,
    /// Raw concluded items, failures included. Keyed by node name: a graph
    /// may hold more than one fan-out node, and each gets its own tables.
    pub bronze: BTreeMap<String, Layer>,
    /// Successful items, typed against each node's declared output.
    pub silver: BTreeMap<String, Layer>,
    /// The job's own curated result.
    pub gold: BTreeMap<String, Layer>,
}

/// An RFC 3339 timestamp for `written_at` and for the gold row.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("Rfc3339 formatting cannot fail for a valid OffsetDateTime")
}

/// Write the manifest to `root/manifest.json`.
pub fn write_manifest(root: &Path, manifest: &Manifest) -> Result<PathBuf, WarehouseError> {
    std::fs::create_dir_all(root).map_err(|e| WarehouseError::Create {
        path: root.to_path_buf(),
        source: e,
    })?;
    let path = root.join("manifest.json");
    let body = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&path, body).map_err(|e| WarehouseError::ManifestWrite {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// A [`TableEntry`] describing a batch written at `path` under `root`.
pub fn entry_for(root: &Path, path: &Path, batch: &RecordBatch) -> TableEntry {
    TableEntry {
        path: path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
        rows: batch.num_rows(),
        columns: batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn record(fields: &[(&str, Ty)]) -> Ty {
        Ty::Record(
            fields
                .iter()
                .map(|(n, t)| (n.to_string(), t.clone()))
                .collect(),
        )
    }

    fn row(item: i64, output: Option<serde_json::Value>, error: Option<&str>) -> Row {
        Row {
            node: "extract".into(),
            item,
            status: if output.is_some() {
                "completed"
            } else {
                "failed"
            }
            .into(),
            concluded_at: "2026-08-18T00:00:00Z".into(),
            source_input: Some(format!(r#"{{"path":"doc-{item}.pdf"}}"#)),
            output,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_node_declaring_json_gets_no_silver_table() {
        // The layer means "validated against a declared shape". `Json` names
        // no shape, so there is nothing to validate and claiming otherwise
        // would make silver indistinguishable from bronze.
        assert!(silver_schema(&Ty::Json, &[]).is_none());
        assert!(silver_schema(&Ty::Record(Default::default()), &[]).is_none());
        assert!(silver_schema(&Ty::Text, &[]).is_none());
    }

    #[test]
    fn silver_columns_follow_the_declared_record() {
        let ty = record(&[("title", Ty::Text), ("body", Ty::Text)]);
        let schema = silver_schema(&ty, &[]).expect("a declared record yields a table");
        let names: Vec<_> = schema.fields().iter().map(|f| f.name().clone()).collect();

        // Lineage first, then the declared fields — prefixed, so a block that
        // names a field `model` or `item` cannot collide with lineage.
        assert_eq!(names[0], "job_id");
        assert!(names.contains(&"f_title".to_string()), "{names:?}");
        assert!(names.contains(&"f_body".to_string()), "{names:?}");
        assert!(!names.contains(&"title".to_string()), "{names:?}");
    }

    #[test]
    fn a_record_of_only_unflattenable_fields_gets_no_table() {
        // A record whose every field is a nested list or a handle has nothing
        // to put in a column, and an all-lineage table with no payload is a
        // table nobody can use.
        let ty = record(&[("pages", Ty::List(Box::new(Ty::Text))), ("scan", Ty::Image)]);
        assert!(silver_schema(&ty, &[]).is_none());
    }

    #[test]
    fn bronze_keeps_failures_and_silver_drops_them() {
        // The split that makes the two layers worth having separately: you
        // audit in bronze and you join in silver.
        let rows = vec![
            row(0, Some(serde_json::json!({"title": "A"})), None),
            row(1, None, Some("pdf has no text layer")),
            row(2, Some(serde_json::json!({"title": "C"})), None),
        ];
        let ty = record(&[("title", Ty::Text)]);

        let bronze = bronze_batch(&rows, &lineage()).unwrap();
        assert_eq!(bronze.num_rows(), 3);

        let silver = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        assert_eq!(silver.num_rows(), 2);
    }

    #[test]
    fn a_string_field_is_not_re_encoded_with_its_quotes() {
        // The classic one-layer-too-many bug: every cell reading `"A"` rather
        // than `A`, which survives a glance at the schema and ruins every
        // join downstream.
        assert_eq!(
            cell_text(&serde_json::json!("A")),
            Some("A".to_string()),
            "a JSON string must become its contents"
        );
        assert_eq!(
            cell_text(&serde_json::json!({"n": 1})),
            Some(r#"{"n":1}"#.to_string()),
            "a JSON object keeps its encoding — there is nothing else it could be"
        );
        assert_eq!(cell_text(&serde_json::Value::Null), None);
    }

    #[test]
    fn a_missing_declared_field_is_null_rather_than_a_write_failure() {
        // A block that omits an optional field at item 9,000 of 10,000 must
        // not lose the run.
        let rows = vec![row(0, Some(serde_json::json!({"title": "A"})), None)];
        let ty = record(&[("title", Ty::Text), ("subtitle", Ty::Text)]);
        let silver = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        assert_eq!(silver.num_rows(), 1);
        let column = silver
            .column_by_name("f_subtitle")
            .expect("the declared field is a column even when unpopulated");
        assert!(column.is_null(0), "an absent field reads as null");
    }

    #[test]
    fn a_declared_number_becomes_a_number_column_not_a_string() {
        // The whole reason `Ty::Number` was added: SUM and AVG and range
        // filters have to work without a cast in every query.
        let rows = vec![row(0, Some(serde_json::json!({"pages": 227})), None)];
        let ty = record(&[("pages", Ty::Number)]);
        let schema = silver_schema(&ty, &rows).unwrap();
        assert_eq!(
            schema.field_with_name("f_pages").unwrap().data_type(),
            &DataType::Int64
        );

        let batch = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        let column = batch
            .column_by_name("f_pages")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .expect("an integral number column is Int64, not stringified");
        assert_eq!(column.value(0), 227);
    }

    #[test]
    fn one_fractional_value_makes_the_whole_column_a_float() {
        // Choosing per column, not per cell: a column that is Int64 for the
        // rows that happen to be whole and Float64 for the rest is not a
        // column. One fractional value anywhere decides it.
        let rows = vec![
            row(0, Some(serde_json::json!({"score": 1})), None),
            row(1, Some(serde_json::json!({"score": 0.75})), None),
        ];
        let ty = record(&[("score", Ty::Number)]);
        let schema = silver_schema(&ty, &rows).unwrap();
        assert_eq!(
            schema.field_with_name("f_score").unwrap().data_type(),
            &DataType::Float64
        );

        let batch = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        let column = batch
            .column_by_name("f_score")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::Float64Array>()
            .unwrap();
        assert_eq!((column.value(0), column.value(1)), (1.0, 0.75));
    }

    #[test]
    fn a_declared_bool_becomes_a_boolean_column() {
        let rows = vec![row(0, Some(serde_json::json!({"has_text": false})), None)];
        let ty = record(&[("has_text", Ty::Bool)]);
        let batch = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        let column = batch
            .column_by_name("f_has_text")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::BooleanArray>()
            .expect("a bool column is Boolean, not the string \"false\"");
        assert!(!column.value(0));
    }

    #[test]
    fn a_number_field_a_block_omitted_is_null_not_zero() {
        // Zero is a real page count. Silently substituting it for "absent"
        // would make every downstream average wrong in a way nothing reports.
        let rows = vec![row(0, Some(serde_json::json!({"other": 1})), None)];
        let ty = record(&[("pages", Ty::Number), ("other", Ty::Number)]);
        let batch = silver_batch(&rows, &lineage(), &ty).unwrap().unwrap();
        let column = batch.column_by_name("f_pages").unwrap();
        assert!(column.is_null(0), "an absent number is null, never 0");
    }

    #[test]
    fn every_row_carries_its_own_lineage() {
        // The property the whole denormalized design exists for: hand one
        // file to somebody with no manifest and they can still trace it.
        let rows = vec![row(0, Some(serde_json::json!({"title": "A"})), None)];
        let bronze = bronze_batch(&rows, &lineage()).unwrap();
        for column in [
            "job_id",
            "spec_fingerprint",
            "model",
            "cuttlefish_version",
            "source_input",
        ] {
            let c = bronze
                .column_by_name(column)
                .unwrap_or_else(|| panic!("bronze must carry `{column}`"));
            assert!(!c.is_null(0), "`{column}` must be populated");
        }
    }
}
