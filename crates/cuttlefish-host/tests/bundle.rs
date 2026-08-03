//! Packaging a checked pipeline into a `.cfbundle`, and reading the result
//! back apart well enough to verify it — full execution of a bundle's
//! nested stages is out of scope until nested-subjobs exists.

mod support;

use cuttlefish_host::bundle;
use cuttlefish_host::catalog::{Catalog, ResolutionContext};
use cuttlefish_host::pipeline::{check, resolve_and_load};
use support::block_with;
use wasmtime::Engine;

fn built_bytes(spec_dir: &std::path::Path, catalog: &Catalog, entries: &[&str]) -> Vec<u8> {
    let engine = Engine::default();
    let resolved: Vec<_> = entries
        .iter()
        .map(|e| resolve_and_load(catalog, spec_dir, e, ResolutionContext::Interactive).unwrap())
        .collect();
    let checked = check(&engine, &resolved).unwrap();
    bundle::build(&checked)
}

#[test]
fn a_built_bundle_starts_with_the_magic_and_a_valid_le_manifest_len() {
    let spec_dir = tempfile::tempdir().unwrap();
    let a = block_with(spec_dir.path(), "bundle_a", "text", "text");
    let catalog = Catalog::open(spec_dir.path().join("unused-catalog"));

    let bytes = built_bytes(spec_dir.path(), &catalog, &[&a.to_string_lossy()]);

    assert_eq!(&bytes[0..4], b"CFBD");
    let manifest_len = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes[12..12 + manifest_len]).unwrap();
    assert_eq!(manifest["manifest_version"], 1);
    assert_eq!(manifest["nodes"].as_array().unwrap().len(), 1);
}

#[test]
fn two_builds_of_the_same_pipeline_are_byte_identical() {
    let spec_dir = tempfile::tempdir().unwrap();
    let a = block_with(spec_dir.path(), "det_a", "text", "{out: text}");
    let b = block_with(spec_dir.path(), "det_b", "{out: text}", "text");
    let catalog = Catalog::open(spec_dir.path().join("unused-catalog"));
    let entries = [
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];
    let entries: Vec<&str> = entries.iter().map(String::as_str).collect();

    let first = built_bytes(spec_dir.path(), &catalog, &entries);
    let second = built_bytes(spec_dir.path(), &catalog, &entries);

    assert_eq!(
        first, second,
        "same spec + same catalog state must produce byte-identical bundles"
    );
}

#[test]
fn a_cataloged_stage_s_manifest_node_records_its_exact_resolution() {
    let spec_dir = tempfile::tempdir().unwrap();
    let a = block_with(spec_dir.path(), "res_a", "text", "text");
    let catalog = Catalog::open(spec_dir.path().join("cat"));
    catalog.add("res-a@1", &a, &Engine::default()).unwrap();

    let bytes = built_bytes(spec_dir.path(), &catalog, &["res-a@1"]);
    let manifest_len = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes[12..12 + manifest_len]).unwrap();
    assert_eq!(manifest["nodes"][0]["resolved"], "res-a@1");
    assert_eq!(manifest["nodes"][0]["kind"], "block");
}

#[test]
fn a_nested_bundle_stage_is_embedded_inline_and_self_contained() {
    let spec_dir = tempfile::tempdir().unwrap();
    let inner_block = block_with(spec_dir.path(), "inner_a", "text", "text");
    let catalog = Catalog::open(spec_dir.path().join("cat"));

    // Build the inner bundle, catalog it, then build an outer pipeline that
    // references it by name.
    let inner_bytes = built_bytes(spec_dir.path(), &catalog, &[&inner_block.to_string_lossy()]);
    let inner_path = spec_dir.path().join("inner.cfbundle");
    std::fs::write(&inner_path, &inner_bytes).unwrap();
    catalog
        .add("inner@1", &inner_path, &Engine::default())
        .unwrap();

    let outer_bytes = built_bytes(spec_dir.path(), &catalog, &["inner@1"]);
    let manifest_len = u64::from_le_bytes(outer_bytes[4..12].try_into().unwrap()) as usize;
    let manifest: serde_json::Value =
        serde_json::from_slice(&outer_bytes[12..12 + manifest_len]).unwrap();
    let node = &manifest["nodes"][0];
    assert_eq!(node["kind"], "bundle");
    assert_eq!(node["resolved"], "inner@1");

    let offset = node["offset"].as_u64().unwrap() as usize;
    let len = node["len"].as_u64().unwrap() as usize;
    let embedded = &outer_bytes[12 + manifest_len + offset..12 + manifest_len + offset + len];
    assert_eq!(
        embedded,
        inner_bytes.as_slice(),
        "the inner bundle's exact bytes must be embedded"
    );
}
