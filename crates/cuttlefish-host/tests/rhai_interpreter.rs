//! End-to-end tests for the shared Rhai interpreter proc-block
//! (`blocks/rhai-interpreter`), driven through the real host exactly like
//! `runner.rs`'s example-block tests — building the real wasm and running
//! real jobs against it, not mocking the guest.

mod support;

use cuttlefish_abi::JobStatus;
use cuttlefish_core::graph::{Branches, Node, NodeGraph};
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::{ArtifactKind, Catalog, ResolutionContext};
use cuttlefish_host::dag::{check_graph, CheckedNode};
use cuttlefish_host::infer::{InferBackend, InferRequest, InferResult, StubBackend};
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::pipeline::resolve_and_load;
use cuttlefish_host::runner::{run_job, JobSpec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// Build the real `cf-block-rhai-interpreter` crate to wasm32 and return its
/// bytes.
///
/// Built here rather than checked in, same reasoning `runner.rs`'s
/// `example_block()` gives: the fixture cannot drift from the crate that
/// actually ships. Built exactly once per test binary for the same
/// concurrency reason `example_block()` documents.
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

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_rhai_interpreter.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

fn script_node(script_field_value: &str) -> CheckedNode {
    CheckedNode {
        name: "rhai".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            output: cuttlefish_abi::Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: Some(script_field_value.to_string()),
        over: None,
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

#[tokio::test]
async fn a_rhai_scripted_block_computes_pure_output_through_the_real_host() {
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node("#{ doubled: input.n * 2 }")],
        exclusive_to: HashMap::new(),
        // Raw job input, exactly as a real submitter would supply it —
        // `run_stage` (not this test) wraps it in `{__cuttlefish_script,
        // input}` before the guest ever sees it, since a Script-kind
        // node's script is fixed at catalog time, never re-supplied by
        // whoever submits the job.
        input: serde_json::json!({ "n": 21 }),
        caps: Capabilities::new(Vec::new()),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["doubled"], 42);
}

#[tokio::test]
async fn a_rhai_script_can_round_trip_an_infer_call_through_the_real_host() {
    let script = "#{ summary: infer(\"summarize: \" + input.text, 32) }";
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "text": "some document text" }),
        caps: Capabilities::new(Vec::new()),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["summary"], "a stub summary");
}

#[tokio::test]
async fn a_rhai_script_can_parse_json_out_of_a_real_infer_reply() {
    let script = r#"#{ verdict: parse_json(infer("judge this", 16)).verdict }"#;
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(Vec::new()),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(Engine::default()),
        // The stub's reply is word-truncated to max_tokens (16 above), so
        // this stays comfortably under that to prove a genuine round trip,
        // not an accidentally-truncated one.
        Arc::new(cuttlefish_host::infer::StubBackend {
            reply: r#"{"verdict": "pass"}"#.to_string(),
        }),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["verdict"], "pass");
}

/// `open`/`slice` through a real capability grant -- proves a Rhai script
/// can read a file at all, the gap that motivated adding these (a script
/// previously had only `input` and `infer()`; any real document work had
/// to happen entirely outside cuttlefish).
#[tokio::test]
async fn a_rhai_script_can_open_and_slice_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, "hello from a real file").unwrap();

    let script = r#"
        let f = open(input.path);
        let s = slice(f.handle, 0, f.len);
        #{ text: s.text, len: f.len }
    "#;
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": doc.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["text"], "hello from a real file");
    assert_eq!(result["len"], "hello from a real file".len());
}

/// The same capability enforcement a Rust block gets, proven for a Rhai
/// script too -- `open` is capability-checked by the host regardless of
/// which language issued it, since enforcement lives in the generic
/// Command dispatch loop, not per-guest-language code.
#[tokio::test]
async fn a_rhai_script_s_open_is_denied_outside_its_granted_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let doc = outside.path().join("secret.txt");
    std::fs::write(&doc, "should not be readable").unwrap();

    let script = r#"open(input.path); #{ ok: true }"#;
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": doc.to_str().unwrap() }),
        // Granted a real, but unrelated, root -- not `outside`.
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
}

/// The real motivating scenario end to end: a script opens a real file and
/// uses regex to find a section heading that a plain substring search
/// would miss (letters spaced out, as SEC filing HTML sometimes renders
/// them), then slices from there. Previously this whole thing had to
/// happen outside cuttlefish, in a driver script -- with open/slice/regex
/// all wired in, it doesn't.
#[tokio::test]
async fn a_rhai_script_extracts_a_section_by_regex_from_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("filing.txt");
    std::fs::write(
        &doc,
        "Item 1. Business\nWe make things.\n\
         Item 1A. RI SK FACTORS\nOur business faces risks including X, Y, Z.\n\
         Item 1B. Unresolved Staff Comments\nNone.",
    )
    .unwrap();

    let script = r#"
        let f = open(input.path);
        let s = slice(f.handle, 0, f.len);
        let m = regex_find(s.text, "(?i)item\\s*1a\\.?\\s*ri\\s*sk\\s*factors");
        #{ found: m.found, heading: m.text }
    "#;
    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": doc.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["found"], true);
    assert_eq!(result["heading"], "Item 1A. RI SK FACTORS");
}

/// The full real path: catalog a `.rhai` file, resolve it via
/// `resolve_and_load`, check it into a `CheckedNode` via `check_graph`, run
/// a real job against it via `run_job` — asserting the result matches what
/// the script computes from the job's real input, WITHOUT the job
/// submitter ever mentioning `__cuttlefish_script` themselves. That's
/// `run_stage`'s job to inject, proven here end to end rather than by
/// hand-constructing a `CheckedNode` with `script` already set (as the
/// other two tests in this file do, for a narrower unit of coverage).
#[tokio::test]
async fn a_script_node_resolved_from_the_catalog_and_run_through_run_job_gets_its_script_injected()
{
    let tmp = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(tmp.path().join("catalog"));
    let engine = Engine::default();

    let script_path = tmp.path().join("triple.rhai");
    std::fs::write(
        &script_path,
        "//! signature: {n: json} -> {tripled: json}\n#{ tripled: input.n * 3 }\n",
    )
    .unwrap();
    catalog.add("triple@1", &script_path, &engine).unwrap();

    let resolved_input = resolve_and_load(
        &catalog,
        tmp.path(),
        "triple@1",
        ResolutionContext::Interactive,
    )
    .unwrap();

    let mut resolved = HashMap::new();
    resolved.insert("triple".to_string(), resolved_input);

    let graph = NodeGraph {
        nodes: vec![(
            "triple".to_string(),
            Node {
                block: std::path::PathBuf::new(),
                input: None,
                repeat_until: None,
                max_iterations: None,
                over: None,
                accept: Vec::new(),
                on_fail: Vec::new(),
            },
        )],
    };

    let checked = check_graph(&engine, &graph, &Branches::default(), &resolved)
        .expect("a single Script node with a valid signature header must typecheck");

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: checked.nodes,
        exclusive_to: checked.exclusive_to,
        // The real job input — nothing here mentions __cuttlefish_script;
        // run_stage constructs that wrapper on its own from the node's
        // script field.
        input: serde_json::json!({ "n": 14 }),
        caps: Capabilities::new(Vec::new()),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(engine),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["tripled"], 42);
}

/// The binary toolkit end to end: a script opens a real archive it has never
/// seen, identifies it from content, and lists what is inside — without the
/// host learning any new commands. Everything below rides on `slice_bytes`,
/// which already existed.
#[tokio::test]
async fn a_rhai_script_identifies_and_lists_a_real_gzipped_tar() {
    let dir = tempfile::tempdir().unwrap();

    // Build a genuine .tar.gz rather than a fixture, so the script is
    // parsing bytes that a real tool would produce.
    let mut tar = Vec::new();
    for (name, body) in [("notes.txt", "hello"), ("data.bin", "xyz")] {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}\0", body.len());
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..262].copy_from_slice(b"ustar");
        tar.extend(header);
        let mut content = body.as_bytes().to_vec();
        content.resize(512, 0);
        tar.extend(content);
    }
    tar.extend(vec![0u8; 1024]);

    let deflated = miniz_oxide::deflate::compress_to_vec(&tar, 6);
    let mut gz = vec![0x1f, 0x8b, 0x08, 0x00];
    gz.extend_from_slice(&[0u8; 6]);
    gz.extend_from_slice(&deflated);

    let archive = dir.path().join("bundle.tar.gz");
    std::fs::write(&archive, &gz).unwrap();

    // Note what the script does NOT do: trust the file name. It reads the
    // magic bytes, decompresses under an explicit ceiling, and only then
    // parses the tar it actually found.
    let script = r#"
        let f = open(input.path);
        let raw = slice_bytes(f.handle, 0, f.len);
        let outer = identify(raw.bytes_base64);
        let inner_b64 = gunzip(raw.bytes_base64, 1048576);
        let inner = identify(inner_b64);
        let entries = tar_entries(inner_b64);
        let names = [];
        for e in entries { names.push(e.name); }
        #{
            outer: outer.format,
            inner: inner.format,
            count: entries.len(),
            names: names,
            entropy_of_compressed: entropy(raw.bytes_base64),
        }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": archive.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["outer"], "gzip");
    assert_eq!(
        result["inner"], "tar",
        "the decompressed bytes must be identified as tar, which has no \
         leading magic — its marker is at offset 257"
    );
    assert_eq!(result["count"], 2);
    assert_eq!(result["names"][0], "notes.txt");
    assert_eq!(result["names"][1], "data.bin");
    assert!(
        result["entropy_of_compressed"].as_f64().unwrap() > 3.0,
        "compressed bytes should not look like plain text: {result}"
    );
}

/// A decompression bomb must be refused by the declared ceiling, as an
/// ordinary catchable error — not discovered by the guest running out of
/// memory, which is a wasm trap that would kill the whole fan-out item.
#[tokio::test]
async fn a_script_can_catch_a_gunzip_that_exceeds_its_ceiling() {
    let dir = tempfile::tempdir().unwrap();

    // Highly compressible: a megabyte of zeros in a few hundred bytes.
    let payload = vec![0u8; 1024 * 1024];
    let deflated = miniz_oxide::deflate::compress_to_vec(&payload, 9);
    let mut gz = vec![0x1f, 0x8b, 0x08, 0x00];
    gz.extend_from_slice(&[0u8; 6]);
    gz.extend_from_slice(&deflated);
    let bomb = dir.path().join("bomb.gz");
    std::fs::write(&bomb, &gz).unwrap();

    let script = r#"
        let f = open(input.path);
        let raw = slice_bytes(f.handle, 0, f.len);
        let refused = false;
        try {
            gunzip(raw.bytes_base64, 4096);
        } catch (err) {
            refused = true;
        }
        #{ refused: refused, compressed_len: f.len }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": bomb.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["refused"], true,
        "the ceiling must stop it, and as a catchable error so the script \
         can record the finding and carry on"
    );
    assert!(
        result["compressed_len"].as_u64().unwrap() < 4096,
        "the point of the case: a small file that would expand far past the \
         ceiling — {result}"
    );
}

/// Image metadata end to end, with no feature flag and no decoder: a script
/// reads a PNG's dimensions and spots a payload appended after IEND — a file
/// that still renders as a normal image in any viewer.
#[tokio::test]
async fn a_rhai_script_reads_image_metadata_and_finds_an_appended_payload() {
    let dir = tempfile::tempdir().unwrap();

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1024u32.to_be_bytes());
    png.extend_from_slice(&768u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&[0u8; 4]);
    png.extend_from_slice(&0u32.to_be_bytes());
    png.extend_from_slice(b"IEND");
    png.extend_from_slice(&[0u8; 4]);
    png.extend_from_slice(b"a stowaway payload");

    let path = dir.path().join("photo.png");
    std::fs::write(&path, &png).unwrap();

    let script = r#"
        let f = open(input.path);
        let raw = slice_bytes(f.handle, 0, f.len);
        let d = dimensions(raw.bytes_base64);
        let chunks = png_chunks(raw.bytes_base64);
        let trailing = 0;
        for c in chunks { if c.type == "TRAILING" { trailing = c.length; } }
        #{ width: d.width, height: d.height, trailing: trailing }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": path.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["width"], 1024);
    assert_eq!(result["height"], 768);
    assert_eq!(
        result["trailing"], 18,
        "bytes after IEND must be reported — silence here is the bug: {result}"
    );
}

/// The host-side half, which needs a decoder. Gated on the same feature the
/// operation is, so a default build's test run doesn't claim to have
/// exercised something it cannot do.
#[cfg(feature = "image-ops")]
#[tokio::test]
async fn a_rhai_script_resizes_an_image_through_the_real_host() {
    let dir = tempfile::tempdir().unwrap();

    let img = image::RgbImage::from_fn(800, 400, |x, _| image::Rgb([(x % 256) as u8, 10, 20]));
    let path = dir.path().join("big.png");
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

    // The resized image comes back as a *handle*, so the script reads its
    // bytes back through slice_bytes and re-reads the dimensions from the
    // header — proving the transform really happened host-side rather than
    // the handle merely being passed through.
    let script = r#"
        let f = open(input.path);
        let small = image_resize(f.handle, 100, 100);
        let raw = slice_bytes(small.handle, 0, small.len);
        let d = dimensions(raw.bytes_base64);
        #{ width: d.width, height: d.height }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": path.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    // 2:1 fitted into a square box, aspect ratio preserved.
    assert_eq!(result["width"], 100);
    assert_eq!(result["height"], 50);
}

/// `document_text` returns the whole document, and a page walk over the same
/// handle extracts once rather than once per page.
///
/// Measured as a **ratio against a single read**, not against a stopwatch.
/// An absolute bound was tried first and failed on CI at 119s where the same
/// code took 5s locally — the same mistake, and the same fix, as the submit
/// timing test: subtract the machine by comparing two measurements taken on
/// it, rather than guessing a constant that holds everywhere.
///
/// That first failure was not a flake. It caught a real defect: the text was
/// cached, but the page-tree count was then fetched per call via `inspect`,
/// which extracts the whole document to answer `has_text_layer`. The walk
/// stayed quadratic with the cost merely moved.
#[tokio::test]
async fn document_text_reads_the_whole_pdf_and_a_page_walk_extracts_once() {
    let dir = tempfile::tempdir().unwrap();
    let pdf =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/docs/sample.pdf");
    let local = dir.path().join("sample.pdf");
    std::fs::copy(&pdf, &local).unwrap();

    async fn timed(
        script: &str,
        path: &std::path::Path,
        dir: &std::path::Path,
    ) -> (serde_json::Value, std::time::Duration) {
        let (tx, _rx) = mpsc::channel(64);
        let job = JobSpec {
            nodes: vec![script_node(script)],
            exclusive_to: HashMap::new(),
            input: serde_json::json!({ "path": path.to_str().unwrap() }),
            caps: Capabilities::new(vec![dir.to_path_buf()]),
            alternates: Default::default(),
            embedder: None,
            warehouse: None,
        };
        let ledger = cuttlefish_host::ledger::Ledger::open(
            &dir.join(format!("ledger-{}.sqlite", script.len())),
            "fp",
        )
        .unwrap();
        let started = std::time::Instant::now();
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
        let elapsed = started.elapsed();
        assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
        (envelope.result.expect("a result"), elapsed)
    }

    // One read, as the baseline: process start, wasm instantiation, and
    // exactly one extraction.
    let (one, one_took) = timed(
        r#"
        let f = open(input.path);
        let whole = document_text(f.handle);
        #{ len: whole.text.len() }
        "#,
        &local,
        dir.path(),
    )
    .await;

    // Eleven reads of the same handle. Uncached, that is eleven extractions.
    let (many, many_took) = timed(
        r#"
        let f = open(input.path);
        let whole = document_text(f.handle);
        let n = 0;
        for i in [0,0,0,0,0,0,0,0,0,0] {
            n += page_text(f.handle, 0).text.len();
        }
        #{ len: whole.text.len(), walked: n }
        "#,
        &local,
        dir.path(),
    )
    .await;

    let len = one["len"].as_u64().unwrap();
    assert!(len > 0, "the document must have text: {one}");
    // Each read returned the same segment, which also proves page 0 of an
    // unsplit document is the whole document rather than nothing.
    assert_eq!(many["walked"].as_u64().unwrap(), len * 10);

    // Eleven cached reads must cost far less than eleven extractions. Five
    // times the single-read cost is a generous ceiling that still catches an
    // order-of-magnitude regression: uncached, this would be about eleven.
    assert!(
        many_took < one_took * 5,
        "eleven reads took {many_took:?} against {one_took:?} for one — that ratio \
         says the per-handle extraction is happening again per call"
    );
}

/// A backend that reports how many images it was handed.
///
/// The assertion that matters for the vision path is not what a model says
/// but whether the image reached it at all: a dropped image produces a
/// confident answer about nothing, which is indistinguishable downstream
/// from a real one.
#[derive(Default)]
struct ImageCountingBackend;

#[async_trait::async_trait]
impl InferBackend for ImageCountingBackend {
    async fn infer(
        &self,
        req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        Ok(InferResult {
            text: format!("saw {} image(s)", req.images.len()),
            tokens_in: 0,
            tokens_out: 0,
        })
    }
    fn model_name(&self) -> String {
        "image-counting".into()
    }
    fn supports_images(&self) -> bool {
        true
    }
}

/// A script can hand an image to a vision model.
///
/// This is the last link in the scanned-document path: `open` reports
/// `has_text_layer: false`, `page_image` renders the page, and this sends it
/// to a model. Before `infer_with_images` existed a script could do the
/// first two and then had nowhere to go — OCR was reachable only from a Rust
/// block, which needs a wasm32 toolchain that users repeatedly lack.
#[tokio::test]
async fn a_script_can_send_an_image_to_a_vision_model() {
    let dir = tempfile::tempdir().unwrap();
    // A real PNG, so the host classifies it as an image and the handle is
    // the same kind a rendered page produces.
    let png = dir.path().join("page.png");
    std::fs::write(
        &png,
        [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0,
            0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
        ],
    )
    .unwrap();

    let script = r#"
        let f = open(input.path);
        #{ answer: infer_with_images("Read this page.", 64, [f.handle]) }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": png.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(ImageCountingBackend),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["answer"], "saw 1 image(s)",
        "the image must reach the backend, not be silently dropped: {result}"
    );
}

#[tokio::test]
async fn passing_something_that_is_not_a_handle_says_so() {
    // A dropped or malformed image is the failure that hides: the model
    // answers about nothing and sounds certain. Naming it beats guessing.
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("x.txt");
    std::fs::write(&doc, "hello").unwrap();

    let script = r#"
        let f = open(input.path);
        #{ answer: infer_with_images("Read it.", 16, ["not-a-handle"]) }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "path": doc.to_str().unwrap() }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(ImageCountingBackend),
        job,
        tx,
        CancellationToken::new(),
        &ledger,
        &ModuleCache::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let message = envelope.error.unwrap().message;
    assert!(
        message.contains("not a handle"),
        "must name the mistake: {message}"
    );
}

/// A script fetches a URL and reads it, with no download step outside the
/// pipeline.
///
/// This is the gap that sent a real run into 120 lines of Python before
/// cuttlefish saw a byte: the corpus was on the web, and nothing here could
/// reach it. Work done outside the pipeline gets none of what the pipeline
/// provides — no capability check, no per-item isolation, no resume.
///
/// Served from a local socket rather than the internet, so the test is
/// hermetic and still exercises the real client, the real capability check,
/// and the real handle path.
#[tokio::test]
async fn a_script_can_fetch_a_url_and_read_it_like_a_file() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // One tiny HTTP/1.1 response, by hand — this needs no server crate.
        while let Ok((mut sock, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = "transmittal R123 nephrology ESRD";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let base = format!("http://127.0.0.1:{port}/");
    let script = r#"
        let f = fetch(input.url);
        let s = slice(f.handle, 0, f.len);
        #{ len: f.len, text: s.text }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({ "url": format!("{base}transmittals") }),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]).with_fetch(vec![base.clone()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["text"], "transmittal R123 nephrology ESRD");
    assert_eq!(result["len"], 32);
}

/// A URL outside every granted prefix is refused, and the message says how
/// to grant it.
#[tokio::test]
async fn fetching_outside_the_granted_prefix_is_denied_with_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"#{ out: fetch("https://elsewhere.test/secrets").len }"#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps: Capabilities::new(vec![dir.path().to_path_buf()])
            .with_fetch(vec!["https://www.cms.gov/".into()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let error = envelope.error.unwrap();
    assert_eq!(error.code, "capability_denied");
    assert!(error.message.contains("Fetch"), "{}", error.message);
    // Naming what *was* granted turns a guess into a comparison.
    assert!(
        error.message.contains("https://www.cms.gov/"),
        "{}",
        error.message
    );
}

/// A script embeds text through the real host.
///
/// Uses a stub embedder rather than Ollama so the test is hermetic; the
/// real backend is exercised separately in `tests/ollama.rs`. What this
/// pins is the path: builtin -> Command::Embed -> backend -> vectors back
/// into script scope, and that the batch form stays batched rather than
/// becoming N round trips.
#[derive(Default)]
struct CountingEmbedder {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl InferBackend for CountingEmbedder {
    async fn infer(
        &self,
        _req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        anyhow::bail!("this backend only embeds")
    }
    fn model_name(&self) -> String {
        "counting-embedder".into()
    }
    fn supports_embeddings(&self) -> bool {
        true
    }
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Deterministic and length-derived, so a test can tell vectors apart.
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32, 1.0, 2.0])
            .collect())
    }
}

#[tokio::test]
async fn a_script_embeds_a_batch_in_one_call() {
    let dir = tempfile::tempdir().unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let embedder: Arc<dyn InferBackend> = Arc::new(CountingEmbedder {
        calls: calls.clone(),
    });

    let script = r#"
        let out = embed_many(["alpha", "bravo bravo", "charlie charlie charlie"]);
        let dims = [];
        for v in out.vectors { dims.push(v[0]); }
        #{ count: out.vectors.len(), first_dims: dims }
    "#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: Some(embedder),
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(result["count"], 3);
    // Vectors come back in input order — the property that lets a caller
    // pair each one with its text without guessing.
    assert_eq!(result["first_dims"][0], 5.0);
    assert_eq!(result["first_dims"][1], 11.0);
    assert_eq!(result["first_dims"][2], 23.0);
    // One batch, not three round trips. This is the whole reason the batch
    // form is the primitive.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn embedding_without_a_declared_model_names_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"#{ n: embed("anything").vectors.len() }"#;

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let message = envelope.error.unwrap().message;
    assert!(message.contains("embedding_model"), "{message}");
}

/// A granted path that names nothing fails as a missing file, not a denial.
///
/// The friction this removes was measured on a real corpus: one manifest
/// entry naming a moved file failed with "read not permitted", which reads as
/// a permissions problem and sends you to re-check the `capabilities` line
/// that was never wrong.
#[tokio::test]
async fn a_missing_file_inside_a_grant_fails_as_missing_not_denied() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("moved-away.txt");
    let script = format!(
        r#"#{{ len: open("{}").len }}"#,
        absent.to_string_lossy().replace('\\', "\\\\")
    );

    let (tx, _rx) = mpsc::channel(64);
    let job = JobSpec {
        nodes: vec![script_node(&script)],
        exclusive_to: HashMap::new(),
        input: serde_json::json!({}),
        caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        alternates: Default::default(),
        embedder: None,
        warehouse: None,
    };
    let ledger =
        cuttlefish_host::ledger::Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();

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

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let error = envelope.error.unwrap();
    // The code matters as much as the words: a script or an agent branching
    // on `capability_denied` would retry with a wider grant, which cannot
    // help and hides the real cause.
    assert_eq!(error.code, "not_found", "{}", error.message);
    assert!(error.message.contains("no such file"), "{}", error.message);
    assert!(
        error.message.contains("rather than the grant"),
        "the message has to say which of the two things is wrong: {}",
        error.message
    );
}

/// A path outside every grant still refuses without saying whether it exists.
#[tokio::test]
async fn a_path_outside_every_grant_is_denied_without_revealing_existence() {
    let granted = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let real = elsewhere.path().join("real.txt");
    std::fs::write(&real, b"secret").unwrap();

    // Same job shape twice: once against a file that is there, once against
    // one that is not. Both must fail identically — any difference is an
    // existence oracle for paths the job was never granted.
    let mut seen = Vec::new();
    for target in [real, elsewhere.path().join("imaginary.txt")] {
        let script = format!(
            r#"#{{ len: open("{}").len }}"#,
            target.to_string_lossy().replace('\\', "\\\\")
        );
        let (tx, _rx) = mpsc::channel(64);
        let job = JobSpec {
            nodes: vec![script_node(&script)],
            exclusive_to: HashMap::new(),
            input: serde_json::json!({}),
            caps: Capabilities::new(vec![granted.path().to_path_buf()]),
            alternates: Default::default(),
            embedder: None,
            warehouse: None,
        };
        let ledger = cuttlefish_host::ledger::Ledger::open(
            &granted.path().join(format!("{}.sqlite", seen.len())),
            "fp",
        )
        .unwrap();
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
        assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
        seen.push(envelope.error.unwrap().code);
    }

    assert_eq!(
        seen[0], seen[1],
        "an absent file and a real one outside the grant must be \
         indistinguishable: {seen:?}"
    );
    assert_eq!(seen[0], "capability_denied");
}
