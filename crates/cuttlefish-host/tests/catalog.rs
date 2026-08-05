//! End-to-end catalog tests using a genuinely-compiled block, so a
//! `signature()` declaration is proven to travel through `add`/`show` rather
//! than assumed — the same reason `pipeline.rs`'s tests compile real
//! fixtures instead of mocking signatures.

mod support;

use cuttlefish_host::catalog::{validate_block_name, ArtifactKind, Catalog};
use support::block_with;
use wasmtime::Engine;

#[test]
fn a_real_block_s_declared_signature_survives_add_and_show() {
    let src_dir = tempfile::tempdir().unwrap();
    let catalog_dir = tempfile::tempdir().unwrap();
    let wasm_path = block_with(
        src_dir.path(),
        "digest_block",
        "{path: text}",
        "{summary: text}",
    );

    let catalog = Catalog::open(catalog_dir.path());
    let engine = Engine::default();
    let outcome = catalog
        .add("digest-block@1", &wasm_path, &engine)
        .expect("a real, correctly-declared block should catalog cleanly");

    assert_eq!(outcome.signature, "{path: text} -> {summary: text}");
    assert!(!outcome.is_permissive_default);

    let shown = catalog
        .show("digest-block@1")
        .expect("just-added entry must be visible to show");
    assert_eq!(shown.signature, "{path: text} -> {summary: text}");
    assert_eq!(shown.kind, ArtifactKind::Block);
}

#[test]
fn concurrent_add_of_different_blocks_never_corrupts_the_index() {
    let src_dir = tempfile::tempdir().unwrap();
    let catalog_dir = tempfile::tempdir().unwrap();
    let first = block_with(src_dir.path(), "concurrent_a", "text", "text");
    let second = block_with(src_dir.path(), "concurrent_b", "text", "text");
    let root = catalog_dir.path().to_path_buf();

    let root_a = root.clone();
    let t1 = std::thread::spawn(move || {
        Catalog::open(&root_a)
            .add("concurrent-a@1", &first, &Engine::default())
            .unwrap();
    });
    let root_b = root.clone();
    let t2 = std::thread::spawn(move || {
        Catalog::open(&root_b)
            .add("concurrent-b@1", &second, &Engine::default())
            .unwrap();
    });
    t1.join().unwrap();
    t2.join().unwrap();

    let entries = Catalog::open(&root).list().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "both concurrent adds must land: {entries:?}"
    );
}

#[test]
fn a_simple_lowercase_name_is_valid() {
    assert!(validate_block_name("my-block").is_ok());
}

#[test]
fn a_name_with_a_dot_is_rejected() {
    let err = validate_block_name("my.block").unwrap_err();
    assert!(err.to_string().contains('.'), "{err}");
}

#[test]
fn a_name_starting_with_a_digit_is_rejected() {
    assert!(validate_block_name("1block").is_err());
}

#[test]
fn a_windows_reserved_device_name_is_rejected_case_insensitively() {
    for bad in ["con", "CON", "Con", "aux", "nul", "com1", "lpt9"] {
        assert!(
            validate_block_name(bad).is_err(),
            "{bad} should be rejected"
        );
    }
}

#[test]
fn a_name_that_only_resembles_a_reserved_name_is_accepted() {
    assert!(validate_block_name("console").is_ok());
    assert!(validate_block_name("commander").is_ok());
}
