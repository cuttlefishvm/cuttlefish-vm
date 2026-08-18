//! The sheet-extract block, driven through the real host over a genuine XLSX.
//!
//! The workbook is built here as an actual zip of actual XML rather than
//! checked in as a fixture blob, so the zip reader, the XML parser and the
//! cell-type mapping are all genuinely exercised — and so the test says out
//! loud what an XLSX *is*, which is the fact that makes this block
//! necessary at all.
//!
//! Entries are stored uncompressed. That keeps the test from needing a zip
//! writer dependency, and `calamine` reads stored entries the same as
//! deflated ones.

mod support;

use cuttlefish_abi::JobStatus;
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::CheckedNode;
use cuttlefish_host::infer::{InferBackend, InferRequest, InferResult};
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::runner::{run_job, JobSpec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

fn block_wasm() -> Vec<u8> {
    static WASM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    WASM.get_or_init(|| {
        let status = support::clean_cargo(env!("CARGO"))
            .args([
                "build",
                "-p",
                "cf-block-sheet-extract",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building the sheet-extract block failed");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_sheet_extract.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

/// A stored-only zip containing `files`.
fn zip(files: &[(&str, &str)]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        // Table-free CRC-32, since the whole point is to avoid a dependency.
        let mut crc = 0xffff_ffffu32;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, body) in files {
        let offset = out.len() as u32;
        let (crc, len) = (crc32(body.as_bytes()), body.len() as u32);

        for (buf, sig) in [(&mut out, 0x0403_4b50u32), (&mut directory, 0x0201_4b50)] {
            buf.extend_from_slice(&sig.to_le_bytes());
            if sig == 0x0201_4b50 {
                buf.extend_from_slice(&20u16.to_le_bytes()); // version made by
            }
            buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
            buf.extend_from_slice(&0u16.to_le_bytes()); // flags
            buf.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            buf.extend_from_slice(&0u32.to_le_bytes()); // time/date
            buf.extend_from_slice(&crc.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes()); // compressed
            buf.extend_from_slice(&len.to_le_bytes()); // uncompressed
            buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
            if sig == 0x0201_4b50 {
                buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
                buf.extend_from_slice(&0u16.to_le_bytes()); // disk
                buf.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
                buf.extend_from_slice(&0u32.to_le_bytes()); // external attrs
                buf.extend_from_slice(&offset.to_le_bytes());
            }
            buf.extend_from_slice(name.as_bytes());
        }
        out.extend_from_slice(body.as_bytes());
    }

    let (dir_offset, dir_len) = (out.len() as u32, directory.len() as u32);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with cd
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&dir_len.to_le_bytes());
    out.extend_from_slice(&dir_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// A workbook shaped like the ones that motivated this block: a header row
/// of names, then numeric rows.
fn workbook() -> Vec<u8> {
    zip(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>member_months</t></is></c><c r="B1" t="inlineStr"><is><t>cost</t></is></c></row><row r="2"><c r="A2"><v>1200</v></c><c r="B2"><v>4567.89</v></c></row><row r="3"><c r="A3"><v>1300</v></c><c r="B3"><v>5000</v></c></row></sheetData></worksheet>"#,
        ),
    ])
}

#[derive(Default)]
struct StubBackend;

#[async_trait::async_trait]
impl InferBackend for StubBackend {
    async fn infer(
        &self,
        _req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        Ok(InferResult {
            text: "unused".into(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }
    fn model_name(&self) -> String {
        "stub".into()
    }
}

async fn run(input: serde_json::Value, dir: &std::path::Path) -> cuttlefish_abi::Envelope {
    let node = CheckedNode {
        name: "sheets".into(),
        kind: ArtifactKind::Block,
        resolved: None,
        module_bytes: block_wasm(),
        signature: cuttlefish_abi::Signature {
            input: "{path: text}".parse().unwrap(),
            output: "{path: text, sheets: json, truncated: json}"
                .parse()
                .unwrap(),
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: None,
        over: None,
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    };
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![node],
        exclusive_to: HashMap::new(),
        input,
        caps: Capabilities::new(vec![dir.to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger = cuttlefish_host::ledger::Ledger::open(&dir.join("ledger.sqlite"), "fp").unwrap();
    run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await
}

#[tokio::test]
async fn a_workbook_the_text_path_cannot_read_becomes_json_rows() {
    // The gap this block closes: an XLSX opens as `binary` with zero
    // characters, so no amount of slice/page_text reaches the numbers.
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("budget.xlsx");
    std::fs::write(&book, workbook()).unwrap();

    let envelope = run(
        serde_json::json!({ "path": book.to_str().unwrap() }),
        dir.path(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.unwrap();

    let rows = &result["sheets"]["Budget"];
    assert_eq!(rows[0][0], "member_months", "{result}");
    // Numbers stay numbers. A downstream node summing a column must not
    // have to reparse strings, and a stringified number is exactly the kind
    // of silently-wrong value that still reads as data.
    assert_eq!(rows[1][0], 1200.0);
    assert_eq!(rows[1][1], 4567.89);
    assert_eq!(rows[2][1], 5000.0);
}

#[tokio::test]
async fn truncation_is_reported_rather_than_silent() {
    // A caller summing a column has to know the column was cut off, or its
    // total is wrong and looks right.
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("budget.xlsx");
    std::fs::write(&book, workbook()).unwrap();

    let envelope = run(
        serde_json::json!({ "path": book.to_str().unwrap(), "max_rows": 2 }),
        dir.path(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.unwrap();

    assert_eq!(result["sheets"]["Budget"].as_array().unwrap().len(), 2);
    assert_eq!(result["truncated"]["Budget"]["returned"], 2);
    assert_eq!(result["truncated"]["Budget"]["total"], 3);
}

#[tokio::test]
async fn a_file_that_is_not_a_workbook_fails_this_item_and_says_why() {
    // A data-quality fact about one file, not an authoring error: it must
    // fail the item with something actionable so a fan-out run continues.
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("not-a-book.xlsx");
    // Genuinely non-UTF-8, or the host classifies it as Text and the block
    // rejects it on kind before ever reaching the parser — a correct
    // failure, but not the one under test here.
    std::fs::write(&junk, [b'P', b'K', 0x03, 0x04, 0xff, 0xfe, 0x00, 0x9c]).unwrap();

    let envelope = run(
        serde_json::json!({ "path": junk.to_str().unwrap() }),
        dir.path(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let message = envelope.error.unwrap().message;
    assert!(
        message.contains("not a readable workbook"),
        "the failure must name the cause: {message}"
    );
}

#[tokio::test]
async fn a_text_file_is_turned_away_before_any_parsing() {
    // The other half of the same guard, and a distinct message: reading a
    // text file badly is worse than declining it, so the block says which
    // tool to reach for instead.
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.xlsx");
    std::fs::write(&notes, "this is prose with a misleading extension").unwrap();

    let envelope = run(
        serde_json::json!({ "path": notes.to_str().unwrap() }),
        dir.path(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let message = envelope.error.unwrap().message;
    assert!(
        message.contains("page_text"),
        "must name the alternative: {message}"
    );
}

/// A workbook shaped the way real ones are: a title, a blank row, then the
/// header — with the data inset one column from A.
fn realistic_workbook() -> Vec<u8> {
    let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="B1" t="inlineStr"><is><t>Demonstration Budget Neutrality</t></is></c></row><row r="2"><c r="B2" t="inlineStr"><is><t></t></is></c></row><row r="3"><c r="B3" t="inlineStr"><is><t>member_months</t></is></c><c r="C3" t="inlineStr"><is><t>cost</t></is></c></row><row r="4"><c r="B4"><v>1200</v></c><c r="C4"><v>4567.89</v></c></row><row r="5"><c r="B5"><v>1300</v></c><c r="C5" t="inlineStr"><is><t>N/A</t></is></c></row></sheetData></worksheet>"#;
    zip(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}

#[tokio::test]
async fn schema_discovery_finds_the_header_below_a_title_and_types_the_columns() {
    // The case that makes naive readers wrong: the header is not row 0, the
    // table is inset from column A, and one column is numbers with a stray
    // "N/A". A caller assuming A1 gets column names that are really a title.
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("real.xlsx");
    std::fs::write(&book, realistic_workbook()).unwrap();

    let envelope = run(
        serde_json::json!({ "path": book.to_str().unwrap() }),
        dir.path(),
    )
    .await;
    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let schema = &envelope.result.unwrap()["schema"]["Budget"];

    assert_eq!(schema["header_row"], 2, "the title must not win: {schema}");
    assert_eq!(schema["data_start_row"], 3);

    let columns = schema["columns"].as_array().unwrap();
    assert_eq!(columns[0]["name"], "member_months");
    assert_eq!(columns[0]["type"], "number");
    // Letters come from the real grid, so a finding can be pointed at in
    // Excel without arithmetic — the data starts at B, not A.
    assert_eq!(columns[0]["letter"], "B");

    assert_eq!(columns[1]["name"], "cost");
    assert_eq!(
        columns[1]["type"], "mixed",
        "a numeric column with a stray N/A is mixed; calling it number is \
         how a total comes out wrong and plausible: {schema}"
    );
}
