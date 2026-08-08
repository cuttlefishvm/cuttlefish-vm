//! End-to-end tests for fan-out (`over`) execution, driven through the real
//! host exactly like `rhai_interpreter.rs` — real wasm, real ledger, real
//! jobs. Nothing mocked.
//!
//! The property these exist to protect: a campaign finishes without the
//! agent that started it. That means a bad item must not kill the run, and a
//! resumed run must not repeat work it already concluded.

mod support;

use cuttlefish_abi::JobStatus;
use cuttlefish_host::caps::Capabilities;
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::{fanout_collection_ty, CheckedNode};
use cuttlefish_host::infer::StubBackend;
use cuttlefish_host::ledger::Ledger;
use cuttlefish_host::module_cache::ModuleCache;
use cuttlefish_host::runner::{run_job, JobSpec};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

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

/// A fan-out node running `script` once per line of `manifest`.
fn map_node(script: &str, manifest: &Path) -> CheckedNode {
    CheckedNode {
        name: "map".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            // Declared per-item output; downstream sees the collection
            // record instead, which is what `fanout_collection_ty` is.
            output: cuttlefish_abi::Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: Some(script.to_string()),
        over: Some(manifest.to_path_buf()),
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

/// A reduce node consuming the collection record: opens `results_path` and
/// counts the lines it actually contains.
fn reduce_node() -> CheckedNode {
    // Note: no `trim()` here. Rhai's `trim` mutates its receiver in place and
    // returns unit, so `line.trim() != ""` compares `()` against `""` — always
    // true — and would count the empty element that trailing-newline splits
    // produce. Comparing the line directly avoids the trap entirely.
    const COUNT_LINES: &str = r#"
        let f = open(input.results_path);
        let s = slice(f.handle, 0, f.len);
        let n = 0;
        for line in s.text.split("\n") {
            if line != "" { n += 1; }
        }
        #{ counted: n }
    "#;
    CheckedNode {
        name: "reduce".to_string(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: interpreter_wasm(),
        signature: cuttlefish_abi::Signature {
            input: fanout_collection_ty(),
            output: cuttlefish_abi::Ty::Json,
        },
        input: Some(cuttlefish_core::graph::InputExpr::FromNode("map".into())),
        repeat_until: None,
        max_iterations: None,
        script: Some(COUNT_LINES.to_string()),
        over: None,
        item_output: None,
        accept: Vec::new(),
        on_fail: Vec::new(),
    }
}

fn write_manifest(dir: &Path, lines: &str) -> std::path::PathBuf {
    let path = dir.join("manifest.jsonl");
    std::fs::write(&path, lines).unwrap();
    path
}

/// Run a job in `dir`, granting read access to `dir` itself (the manifest)
/// plus the job's results directory, mirroring what the daemon injects.
async fn run(nodes: Vec<CheckedNode>, dir: &Path) -> cuttlefish_abi::Envelope {
    let ledger_path = dir.join("ledger.sqlite");
    let ledger = Ledger::open(&ledger_path, "fp").unwrap();
    run_on(nodes, dir, &ledger).await
}

async fn run_on(nodes: Vec<CheckedNode>, dir: &Path, ledger: &Ledger) -> cuttlefish_abi::Envelope {
    let (tx, _rx) = mpsc::channel(1024);
    let job = JobSpec {
        nodes,
        exclusive_to: HashMap::new(),
        input: serde_json::Value::Null,
        caps: Capabilities::new(vec![dir.to_path_buf()]),
        alternates: Default::default(),
    };
    run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        job,
        tx,
        CancellationToken::new(),
        ledger,
        &ModuleCache::new(),
    )
    .await
}

#[tokio::test]
async fn a_fan_out_node_runs_once_per_manifest_line_and_a_reduce_reads_the_results() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\n{\"n\": 2}\n{\"n\": 3}\n");

    let envelope = run(
        vec![
            map_node("#{ doubled: input.n * 2 }", &manifest),
            reduce_node(),
        ],
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    let result = envelope.result.expect("a completed job carries a result");
    assert_eq!(
        result["counted"], 3,
        "the reduce must see all three results"
    );
}

#[tokio::test]
async fn one_bad_item_is_recorded_and_the_rest_still_run() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\n{\"n\": 0}\n{\"n\": 3}\n");

    let envelope = run(
        vec![
            map_node(
                "if input.n == 0 { throw \"bad chunk\"; } #{ doubled: input.n * 2 }",
                &manifest,
            ),
            reduce_node(),
        ],
        dir.path(),
    )
    .await;

    assert_eq!(
        envelope.status,
        JobStatus::Completed,
        "a bad chunk must not kill the whole run: {envelope:?}"
    );
    assert_eq!(envelope.result.unwrap()["counted"], 2);

    let failures = std::fs::read_to_string(dir.path().join("results/map.failures.jsonl")).unwrap();
    assert!(failures.contains("\"item\":1"), "{failures}");
    assert!(failures.contains("bad chunk"), "{failures}");
}

#[tokio::test]
async fn an_empty_manifest_fails_the_job() {
    // Zero items is an upstream extraction bug, and reducing over nothing is
    // the silent-degradation case this whole design tries to avoid.
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "");

    let envelope = run(vec![map_node("input", &manifest)], dir.path()).await;

    assert_eq!(envelope.status, JobStatus::Failed);
    let msg = envelope.error.unwrap().message;
    assert!(msg.contains("empty"), "{msg}");
}

#[tokio::test]
async fn a_manifest_line_that_is_not_json_fails_before_any_item_runs() {
    // An unparseable manifest is an authoring error, not a data-quality one.
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\nnot json at all\n");

    let envelope = run(vec![map_node("input", &manifest)], dir.path()).await;

    assert_eq!(envelope.status, JobStatus::Failed);
    let msg = envelope.error.unwrap().message;
    assert!(msg.contains("line 2"), "must name the bad line: {msg}");
}

#[tokio::test]
async fn a_manifest_where_every_item_fails_fails_the_job() {
    // The degenerate case: "proceed over the successes" with no successes.
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 0}\n{\"n\": 0}\n");

    let envelope = run(
        vec![map_node("throw \"always\";", &manifest), reduce_node()],
        dir.path(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
}

#[tokio::test]
async fn resume_does_not_repeat_items_that_already_concluded() {
    // The property the whole feature exists for. Pre-seed the ledger exactly
    // as an interrupted first run would have left it, then assert the
    // already-concluded items are not run again -- via their *recorded*
    // outputs surviving, which a re-run would overwrite with fresh values.
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\n{\"n\": 2}\n{\"n\": 3}\n");
    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(std::fs::read(&manifest).unwrap());
        format!("{:x}", h.finalize())
    };

    let ledger_path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&ledger_path, "fp").unwrap();
    ledger
        .check_or_record_manifest("map", &digest, 3)
        .unwrap()
        .unwrap();
    // A sentinel value the script could never produce -- if item 0 were
    // re-run, this would be replaced by {"doubled": 2}.
    ledger
        .write_item_completed("map", 0, &serde_json::json!({"sentinel": true}))
        .unwrap();
    ledger.write_item_failed("map", 1, "bad chunk").unwrap();

    let envelope = run_on(
        vec![
            map_node("#{ doubled: input.n * 2 }", &manifest),
            reduce_node(),
        ],
        dir.path(),
        &ledger,
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed, "{envelope:?}");
    // Two successes: the pre-seeded item 0, plus item 2 which genuinely ran.
    assert_eq!(envelope.result.unwrap()["counted"], 2);

    let results = std::fs::read_to_string(dir.path().join("results/map.results.jsonl")).unwrap();
    assert!(
        results.contains("sentinel"),
        "item 0's recorded output must survive rather than being recomputed: {results}"
    );
}

#[tokio::test]
async fn resuming_against_an_edited_manifest_is_refused() {
    // An item_index only means something relative to one manifest; silently
    // reindexing would pair recorded results with different inputs.
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\n{\"n\": 2}\n");

    let ledger_path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&ledger_path, "fp").unwrap();
    ledger
        .check_or_record_manifest("map", "a-digest-from-a-different-manifest", 2)
        .unwrap()
        .unwrap();

    let envelope = run_on(
        vec![map_node("#{ doubled: input.n * 2 }", &manifest)],
        dir.path(),
        &ledger,
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed, "{envelope:?}");
    let msg = envelope.error.unwrap().message;
    assert!(msg.contains("map"), "must name the node: {msg}");
    assert!(msg.contains("manifest"), "{msg}");
}

/// Built through the *real* `check_graph`, not hand-assembled `CheckedNode`s.
///
/// This exists because a bug slipped past every other test in this file: the
/// typechecker replaces a fan-out node's `signature.output` with the
/// collection record for downstream typing, which meant per-item validation
/// was checking each item against the collection type and rejecting all of
/// them. Tests that construct `CheckedNode` directly bypass that
/// substitution entirely and so cannot see it. Only the full path can.
#[tokio::test]
async fn per_item_validation_uses_the_blocks_own_output_not_the_collection_type() {
    use cuttlefish_core::graph::{Branches, Node, NodeGraph};
    use cuttlefish_host::catalog::{Catalog, ResolutionContext};
    use cuttlefish_host::dag::check_graph;
    use cuttlefish_host::pipeline::resolve_and_load;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::default();
    let catalog = Catalog::open(dir.path().join("catalog"));
    let manifest = write_manifest(dir.path(), "{\"n\": 1}\n{\"n\": 2}\n");

    // The map block declares a *specific* per-item output, so if validation
    // used the collection record instead, every item would be rejected.
    let map_src = dir.path().join("double.rhai");
    std::fs::write(
        &map_src,
        "//! signature: json -> {doubled: json}\n#{ doubled: input.n * 2 }\n",
    )
    .unwrap();
    catalog.add("double@1", &map_src, &engine).unwrap();

    let reduce_src = dir.path().join("tally.rhai");
    std::fs::write(
        &reduce_src,
        "//! signature: {results_path: text, failures_path: text, succeeded: json, failed: json} -> json\n\
         let f = open(input.results_path);\n\
         let s = slice(f.handle, 0, f.len);\n\
         let n = 0;\n\
         for line in s.text.split(\"\\n\") { if line != \"\" { n += 1; } }\n\
         #{ counted: n }\n",
    )
    .unwrap();
    catalog.add("tally@1", &reduce_src, &engine).unwrap();

    let mut resolved = HashMap::new();
    for (node, name_version) in [("analyze", "double@1"), ("tally", "tally@1")] {
        resolved.insert(
            node.to_string(),
            resolve_and_load(
                &catalog,
                dir.path(),
                name_version,
                ResolutionContext::Interactive,
            )
            .unwrap(),
        );
    }

    let analyze = Node {
        block: std::path::PathBuf::new(),
        input: None,
        repeat_until: None,
        max_iterations: None,
        over: Some(manifest),
        accept: Vec::new(),
        on_fail: Vec::new(),
    };
    let graph = NodeGraph {
        nodes: vec![
            ("analyze".to_string(), analyze),
            (
                "tally".to_string(),
                Node {
                    block: std::path::PathBuf::new(),
                    input: Some(cuttlefish_core::graph::InputExpr::FromNode(
                        "analyze".into(),
                    )),
                    repeat_until: None,
                    max_iterations: None,
                    over: None,
                    accept: Vec::new(),
                    on_fail: Vec::new(),
                },
            ),
        ],
    };

    let checked = check_graph(&engine, &graph, &Branches::default(), &resolved)
        .expect("a fan-out node feeding a reduce must typecheck");

    let ledger = Ledger::open(&dir.path().join("ledger.sqlite"), "fp").unwrap();
    let envelope = run_on(checked.nodes, dir.path(), &ledger).await;

    assert_eq!(
        envelope.status,
        JobStatus::Completed,
        "items must validate against the block's own declared output: {envelope:?}"
    );
    assert_eq!(envelope.result.unwrap()["counted"], 2);
}
