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
use cuttlefish_host::infer::StubBackend;
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
