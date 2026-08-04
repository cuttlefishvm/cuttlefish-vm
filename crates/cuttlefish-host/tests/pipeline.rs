//! Tests for pipeline typechecking.
//!
//! The mismatch these catch is the one that actually happens when blocks are
//! composed, and its untyped failure mode is bad: a block handed the wrong shape
//! does not fail at the seam, it fails somewhere inside itself with a confusing
//! error about a missing field — or produces a plausible answer computed from
//! nothing.
//!
//! Blocks are compiled here from source rather than mocked, because the whole
//! premise is that a signature comes from the artifact that will run. A mocked
//! signature would test the checker against a fiction.

mod support;

use cuttlefish_host::catalog::{ArtifactKind, Catalog, ResolutionContext};
use cuttlefish_host::pipeline::{
    check, read_stage_signature, resolve_and_load, PipelineError, ResolvedInput,
};
use std::path::PathBuf;
use support::block_with;
use wasmtime::Engine;

/// Load a compiled block straight from disk into a `ResolvedInput`, with no
/// catalog involved — these tests are about seam-checking, not resolution
/// (that's `resolve_and_load`, tested separately once it exists).
fn direct(path: PathBuf) -> ResolvedInput {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    ResolvedInput {
        name,
        kind: ArtifactKind::Block,
        resolved: None,
        bytes: std::fs::read(&path)
            .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display())),
    }
}

/// Build a `ResolvedInput` straight from in-memory bytes with an explicit
/// `kind`, for the `ArtifactKind::Bundle` arm of `check()` — unlike `direct`,
/// there is no on-disk fixture to compile; the bytes are the whole fixture.
fn direct_bytes(name: &str, kind: ArtifactKind, bytes: Vec<u8>) -> ResolvedInput {
    ResolvedInput {
        name: name.to_string(),
        kind,
        resolved: None,
        bytes,
    }
}

/// Build a minimal `.cfbundle` container: magic `b"CFBD"` + an 8-byte
/// little-endian manifest length + the manifest JSON bytes. Mirrors the
/// `make_bundle` fixture helper in `catalog.rs`'s own unit tests (the
/// authoritative source for this container's byte layout) rather than
/// reinventing the layout here.
fn make_bundle(manifest_json: &[u8]) -> Vec<u8> {
    let mut bytes = b"CFBD".to_vec();
    bytes.extend_from_slice(&(manifest_json.len() as u64).to_le_bytes());
    bytes.extend_from_slice(manifest_json);
    bytes
}

#[test]
fn a_pipeline_whose_seams_line_up_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let first = block_with(dir.path(), "seam_ok_a", "{path: text}", "{chunks: [text]}");
    let second = block_with(
        dir.path(),
        "seam_ok_b",
        "{chunks: [text]}",
        "{summary: text}",
    );

    let checked =
        check(&Engine::default(), &[direct(first), direct(second)]).expect("the seams line up");

    assert_eq!(checked.stages().len(), 2);
    assert_eq!(checked.input().to_string(), "{path: text}");
    assert_eq!(checked.output().to_string(), "{summary: text}");
}

#[test]
fn a_mismatched_seam_is_rejected_naming_both_blocks_and_both_types() {
    // The error has to say enough to act on: which two blocks, and what each
    // side wanted. "type error" would send someone reading four files.
    let dir = tempfile::tempdir().unwrap();
    let producer = block_with(dir.path(), "seam_bad_a", "{path: text}", "{summary: text}");
    let consumer = block_with(dir.path(), "seam_bad_b", "{chunks: [text]}", "{out: text}");

    let err = check(&Engine::default(), &[direct(producer), direct(consumer)])
        .err()
        .expect("a mismatched seam must be rejected");

    assert!(matches!(err, PipelineError::SeamMismatch { .. }), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("seam_bad_a"), "names the producer: {msg}");
    assert!(msg.contains("seam_bad_b"), "names the consumer: {msg}");
    assert!(
        msg.contains("{summary: text}"),
        "shows what was produced: {msg}"
    );
    assert!(
        msg.contains("{chunks: [text]}"),
        "shows what was expected: {msg}"
    );
}

#[test]
fn a_producer_may_add_fields_its_consumer_does_not_need() {
    // Extra output must not break a downstream block; otherwise every block
    // gaining a field breaks every pipeline it is in.
    let dir = tempfile::tempdir().unwrap();
    let wide = block_with(dir.path(), "wide_a", "{path: text}", "{a: text, b: text}");
    let narrow = block_with(dir.path(), "narrow_b", "{a: text}", "{out: text}");

    assert!(check(&Engine::default(), &[direct(wide), direct(narrow)]).is_ok());
}

#[test]
fn a_json_seam_accepts_anything() {
    // `json` is the top type and the escape hatch. It is also, deliberately, the
    // default — an unannotated block composes, just without its seam checked.
    let dir = tempfile::tempdir().unwrap();
    let typed = block_with(dir.path(), "json_a", "{path: text}", "{a: text}");
    let loose = block_with(dir.path(), "json_b", "json", "{out: text}");

    assert!(check(&Engine::default(), &[direct(typed), direct(loose)]).is_ok());
}

#[test]
fn a_specific_input_does_not_accept_json() {
    // The other direction must fail, or `json` anywhere would silently disable
    // checking for everything downstream of it.
    let dir = tempfile::tempdir().unwrap();
    let loose = block_with(dir.path(), "rev_a", "{path: text}", "json");
    let typed = block_with(dir.path(), "rev_b", "{needed: text}", "{out: text}");

    assert!(check(&Engine::default(), &[direct(loose), direct(typed)]).is_err());
}

#[test]
fn a_single_block_pipeline_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let only = block_with(dir.path(), "single_a", "{path: text}", "{summary: text}");

    let checked =
        check(&Engine::default(), &[direct(only)]).expect("one block is a valid pipeline");
    assert_eq!(checked.stages().len(), 1);
    // With one stage the pipeline's type is that stage's type.
    assert_eq!(checked.input().to_string(), "{path: text}");
    assert_eq!(checked.output().to_string(), "{summary: text}");
}

#[test]
fn a_bundle_stage_is_checked_from_its_cached_manifest_signature() {
    // A bundle carries no wasm to inspect directly; its signature comes from
    // the manifest JSON `read_bundle_signature` pulls out. This is the
    // Bundle-arm counterpart of `a_pipeline_whose_seams_line_up_is_accepted`.
    let bundle =
        make_bundle(br#"{"nodes":[],"edges":[],"signature":"{path: text} -> {summary: text}"}"#);

    let checked = check(
        &Engine::default(),
        &[direct_bytes("my_bundle", ArtifactKind::Bundle, bundle)],
    )
    .expect("a well-formed bundle manifest checks fine");

    assert_eq!(checked.stages().len(), 1);
    assert_eq!(checked.input().to_string(), "{path: text}");
    assert_eq!(checked.output().to_string(), "{summary: text}");
}

#[test]
fn an_uninspectable_bundle_names_the_stage_exactly_once() {
    // `read_bundle_signature`'s own error already embeds the stage's name
    // (it uses the name as the artifact's display path), so `check()`
    // wrapping that error's *full* text into `Uninspectable`'s `{name}:
    // {message}` would print the name twice: "inspecting my_bundle:
    // my_bundle: shorter than the bundle header". The name must appear once.
    let too_short = b"CFBD".to_vec(); // shorter than the 12-byte bundle header

    let err = check(
        &Engine::default(),
        &[direct_bytes("my_bundle", ArtifactKind::Bundle, too_short)],
    )
    .err()
    .expect("a truncated bundle header must be rejected");

    assert!(
        matches!(err, PipelineError::Uninspectable { .. }),
        "{err:?}"
    );
    let msg = err.to_string();
    assert_eq!(
        msg.matches("my_bundle").count(),
        1,
        "the stage name must appear exactly once: {msg}"
    );
    assert!(
        msg.contains("shorter than the bundle header"),
        "the underlying reason must still be present: {msg}"
    );
}

#[test]
fn an_empty_pipeline_is_rejected() {
    let err = check(&Engine::default(), &[])
        .err()
        .expect("nothing to run");
    assert!(matches!(err, PipelineError::Empty));
}

#[test]
fn read_stage_signature_matches_what_check_already_produced_for_a_block() {
    // `read_stage_signature` must be the exact same per-node lookup `check`
    // already runs — pinning it against a real compiled block, not a mock,
    // is the whole point (a mocked signature would test the extraction
    // against a fiction rather than the artifact that will actually run).
    let dir = tempfile::tempdir().unwrap();
    let block = block_with(dir.path(), "read_sig_a", "text", "text");

    let input = ResolvedInput {
        name: "x".into(),
        kind: ArtifactKind::Block,
        resolved: None,
        bytes: std::fs::read(&block).unwrap(),
    };

    let sig = read_stage_signature(&Engine::default(), &input).unwrap();
    assert_eq!(sig.input.describe(), "text");
    assert_eq!(sig.output.describe(), "text");
}

// -- execution --------------------------------------------------------------

/// Compile a block that echoes its input with a field added, so a chain's
/// stages can be told apart in the result.
fn tagging_block(dir: &std::path::Path, name: &str, tag: &str) -> PathBuf {
    let crate_dir = dir.join(name);
    std::fs::create_dir_all(crate_dir.join("src")).unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/cuttlefish-sdk");

    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n\
             cuttlefish-sdk = {{ path = '{}' }}\nserde_json = \"1\"\n\n[workspace]\n",
            sdk.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    std::fs::write(
        crate_dir.join("src/lib.rs"),
        format!(
            r#"use cuttlefish_sdk::{{export_block, Block, Command, Event}};

#[derive(Default)]
struct B;

impl Block for B {{
    fn start(&mut self, input: serde_json::Value) -> Command {{
        let mut seen: Vec<String> = input
            .get("seen")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        seen.push("{tag}".to_string());
        Command::Done {{ result: serde_json::json!({{ "seen": seen }}) }}
    }}
    fn step(&mut self, _event: Event) -> Command {{
        Command::Fail {{ code: "unexpected".into(), message: "no commands issued".into() }}
    }}
}}

export_block!(B);
"#
        ),
    )
    .unwrap();

    let status = crate::support::clean_cargo(env!("CARGO"))
        .current_dir(&crate_dir)
        .args(["build", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo should start");
    assert!(status.success(), "building {name} failed");

    crate_dir
        .join("target/wasm32-unknown-unknown/debug")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

/// A [`cuttlefish_host::dag::CheckedNode`] for a test, permissive-signature
/// and non-looping, differing only in name, module bytes, and `input`.
fn checked_node(
    name: &str,
    module_bytes: Vec<u8>,
    input: Option<cuttlefish_core::graph::InputExpr>,
) -> cuttlefish_host::dag::CheckedNode {
    cuttlefish_host::dag::CheckedNode {
        name: name.to_string(),
        kind: ArtifactKind::Block,
        resolved: None,
        module_bytes,
        signature: cuttlefish_abi::Signature {
            input: cuttlefish_abi::Ty::Json,
            output: cuttlefish_abi::Ty::Json,
        },
        input,
        repeat_until: None,
        max_iterations: None,
    }
}

#[tokio::test]
async fn a_pipeline_threads_each_result_into_the_next_block() {
    use cuttlefish_core::graph::InputExpr;
    use cuttlefish_host::{
        caps::Capabilities,
        infer::StubBackend,
        runner::{run_job, JobSpec},
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let first = std::fs::read(tagging_block(dir.path(), "chain_one", "first")).unwrap();
    let second = std::fs::read(tagging_block(dir.path(), "chain_two", "second")).unwrap();
    let third = std::fs::read(tagging_block(dir.path(), "chain_three", "third")).unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = cuttlefish_host::ledger::Ledger::open(
        &ledger_dir.path().join("ledger.sqlite"),
        "test-fingerprint",
    )
    .unwrap();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            nodes: vec![
                checked_node("n0", first, None),
                checked_node("n1", second, Some(InputExpr::FromNode("n0".to_string()))),
                checked_node("n2", third, Some(InputExpr::FromNode("n1".to_string()))),
            ],
            exclusive_to: std::collections::HashMap::new(),
            input: serde_json::json!({}),
            caps: Capabilities::default(),
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        &ledger,
    )
    .await;

    assert_eq!(envelope.status, cuttlefish_abi::JobStatus::Completed);
    // Order is the whole meaning of a pipeline: each block saw the previous
    // one's output, and they ran in the order written.
    assert_eq!(
        envelope.result.unwrap()["seen"],
        serde_json::json!(["first", "second", "third"])
    );
}

#[tokio::test]
async fn a_failing_stage_ends_the_job_and_names_the_stage() {
    use cuttlefish_core::graph::InputExpr;
    use cuttlefish_host::{
        caps::Capabilities,
        infer::StubBackend,
        runner::{run_job, JobSpec},
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let good = std::fs::read(tagging_block(dir.path(), "fail_one", "first")).unwrap();

    // A later stage that is not wasm at all: it must fail, and the error must
    // say *which* block, or a long pipeline turns debugging into a hunt.
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = cuttlefish_host::ledger::Ledger::open(
        &ledger_dir.path().join("ledger.sqlite"),
        "test-fingerprint",
    )
    .unwrap();
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            nodes: vec![
                checked_node("n0", good, None),
                checked_node(
                    "n1",
                    b"not wasm".to_vec(),
                    Some(InputExpr::FromNode("n0".to_string())),
                ),
            ],
            exclusive_to: std::collections::HashMap::new(),
            input: serde_json::json!({}),
            caps: Capabilities::default(),
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        &ledger,
    )
    .await;

    assert_eq!(envelope.status, cuttlefish_abi::JobStatus::Failed);
    let message = envelope.error.unwrap().message;
    assert!(
        message.contains("block 2"),
        "the failing stage must be named: {message}"
    );
    assert!(envelope.result.is_none());
}

// -- resolve_and_load ---------------------------------------------------------

#[test]
fn a_relative_direct_entry_resolves_against_spec_dir_not_cwd() {
    let spec_dir = tempfile::tempdir().unwrap();
    let wasm = block_with(spec_dir.path(), "rel_a", "text", "text");
    // The entry as it would appear in a spec file: relative to the spec,
    // not to wherever `cuttlefish` happens to be run from.
    let relative_entry = "rel_a/target/wasm32-unknown-unknown/debug/rel_a.wasm".to_string();
    let catalog = Catalog::open(spec_dir.path().join("unused-catalog"));

    let resolved = resolve_and_load(
        &catalog,
        spec_dir.path(),
        &relative_entry,
        ResolutionContext::Interactive,
    )
    .unwrap();

    assert_eq!(resolved.bytes, std::fs::read(&wasm).unwrap());
    assert_eq!(resolved.resolved, None);
}

#[test]
fn a_bare_catalog_name_is_not_joined_against_spec_dir() {
    let src_dir = tempfile::tempdir().unwrap();
    let catalog_dir = tempfile::tempdir().unwrap();
    let wasm = block_with(src_dir.path(), "cat_a", "text", "text");
    let catalog = Catalog::open(catalog_dir.path());
    catalog.add("cat-a@1", &wasm, &Engine::default()).unwrap();

    // spec_dir deliberately does not contain anything named "cat-a@1" — if
    // the join were applied unconditionally this would fail to resolve.
    let empty_spec_dir = tempfile::tempdir().unwrap();
    let resolved = resolve_and_load(
        &catalog,
        empty_spec_dir.path(),
        "cat-a@1",
        ResolutionContext::Interactive,
    )
    .unwrap();

    assert_eq!(resolved.resolved, Some("cat-a@1".to_string()));
    assert_eq!(resolved.bytes, std::fs::read(&wasm).unwrap());
}

#[test]
fn a_dotted_catalog_name_still_resolves_through_the_catalog_not_a_join() {
    // Regression test: `resolve_and_load` used to decide whether to join an
    // entry against `spec_dir` from the entry's *shape* alone, which could
    // corrupt a catalog name that merely looked path-like into a bogus path
    // before `catalog.resolve` ever got a chance to look it up. A catalog
    // name may legally contain `.` (only `/` is rejected, by
    // `validate_name_version`'s character-class check), so a name like
    // "team.cat-a" is exactly the kind of not-quite-a-filename string this
    // must still resolve correctly, via the catalog's own index lookup
    // rather than a filesystem join.
    let src_dir = tempfile::tempdir().unwrap();
    let catalog_dir = tempfile::tempdir().unwrap();
    let wasm = block_with(src_dir.path(), "team_cat_a", "text", "text");
    let catalog = Catalog::open(catalog_dir.path());
    catalog
        .add("team.cat-a@1", &wasm, &Engine::default())
        .unwrap();

    // spec_dir deliberately has nothing at "team.cat-a@1" — if the entry
    // were joined against it regardless, there'd be no file there and the
    // bug would be masked rather than reproduced.
    let empty_spec_dir = tempfile::tempdir().unwrap();
    let resolved = resolve_and_load(
        &catalog,
        empty_spec_dir.path(),
        "team.cat-a@1",
        ResolutionContext::Interactive,
    )
    .unwrap();

    assert_eq!(resolved.resolved, Some("team.cat-a@1".to_string()));
    assert_eq!(resolved.bytes, std::fs::read(&wasm).unwrap());
}

#[test]
fn an_unqualified_name_in_durable_context_is_rejected() {
    let catalog_dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(catalog_dir.path());
    let spec_dir = tempfile::tempdir().unwrap();

    let err = resolve_and_load(
        &catalog,
        spec_dir.path(),
        "no-version",
        ResolutionContext::Durable,
    )
    .unwrap_err();
    assert!(matches!(err, PipelineError::Resolution(_)));
}

#[test]
fn a_source_directory_is_rejected_with_a_friendly_message() {
    let spec_dir = tempfile::tempdir().unwrap();
    let block_src_dir = spec_dir.path().join("some-block-src");
    std::fs::create_dir(&block_src_dir).unwrap();
    let catalog = Catalog::open(spec_dir.path().join("unused-catalog"));

    // Must contain a `/` so `resolve_and_load`'s own `looks_path_like` rule
    // joins it against `spec_dir` — a bare `"some-block-src"` (no slash, no
    // .wasm/.cfbundle suffix) is treated as a catalog name instead and would
    // return NotFound, not the directory message this test checks for.
    let err = resolve_and_load(
        &catalog,
        spec_dir.path(),
        "./some-block-src",
        ResolutionContext::Interactive,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("directory"), "{msg}");
}

#[test]
fn a_missing_direct_path_names_the_path() {
    let spec_dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(spec_dir.path().join("unused-catalog"));

    let err = resolve_and_load(
        &catalog,
        spec_dir.path(),
        "no/such/block.wasm",
        ResolutionContext::Interactive,
    )
    .unwrap_err();
    assert!(err.to_string().contains("block.wasm"), "{err}");
}
