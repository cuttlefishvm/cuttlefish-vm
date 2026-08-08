//! End-to-end catalog tests using a genuinely-compiled block, so a
//! `signature()` declaration is proven to travel through `add`/`show` rather
//! than assumed — the same reason `pipeline.rs`'s tests compile real
//! fixtures instead of mocking signatures.

mod support;

use cuttlefish_host::catalog::{ArtifactKind, Catalog};
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

use std::io::Write;

fn write_script(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    path
}

#[test]
fn adding_a_rhai_file_catalogs_it_as_script_kind_with_its_declared_signature() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = cuttlefish_host::catalog::Catalog::open(tmp.path().join("catalog"));
    let engine = wasmtime::Engine::default();

    let script_path = write_script(
        tmp.path(),
        "greet.rhai",
        "//! signature: {name: text} -> {greeting: text}\n\
         #{ greeting: \"hello \" + input.name }\n",
    );

    let outcome = catalog.add("greet@1", &script_path, &engine).unwrap();
    assert_eq!(outcome.kind, cuttlefish_host::catalog::ArtifactKind::Script);
    assert_eq!(outcome.signature, "{name: text} -> {greeting: text}");
}

#[test]
fn a_rhai_file_with_no_signature_header_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = cuttlefish_host::catalog::Catalog::open(tmp.path().join("catalog"));
    let engine = wasmtime::Engine::default();
    let script_path = write_script(tmp.path(), "no_sig.rhai", "#{ x: 1 }\n");

    let err = catalog.add("no-sig@1", &script_path, &engine).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("signature"),
        "expected a signature-related error, got: {err}"
    );
}

#[test]
fn a_rhai_file_with_an_unparseable_signature_header_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = cuttlefish_host::catalog::Catalog::open(tmp.path().join("catalog"));
    let engine = wasmtime::Engine::default();
    let script_path = write_script(
        tmp.path(),
        "bad_sig.rhai",
        "//! signature: this is not a real signature\n#{ x: 1 }\n",
    );

    assert!(catalog.add("bad-sig@1", &script_path, &engine).is_err());
}

#[test]
fn a_rhai_file_with_two_signature_headers_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = cuttlefish_host::catalog::Catalog::open(tmp.path().join("catalog"));
    let engine = wasmtime::Engine::default();
    let script_path = write_script(
        tmp.path(),
        "dup_sig.rhai",
        "//! signature: {n: json} -> {n: json}\n\
         //! signature: {n: json} -> {n: json}\n\
         input\n",
    );

    let err = catalog.add("dup-sig@1", &script_path, &engine).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("exactly one"),
        "expected an error about multiple signature headers, got: {err}"
    );
}

#[test]
fn a_rhai_file_with_a_valid_header_but_a_syntax_error_in_its_body_is_rejected_at_add_time() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = cuttlefish_host::catalog::Catalog::open(tmp.path().join("catalog"));
    let engine = wasmtime::Engine::default();
    let script_path = write_script(
        tmp.path(),
        "broken.rhai",
        "//! signature: {n: json} -> {n: json}\nlet x = ;;;\n",
    );

    let err = catalog
        .add("broken@1", &script_path, &engine)
        .expect_err("a script whose body doesn't parse must be rejected at catalog add, not deferred to run time");
    assert!(
        err.to_string().to_lowercase().contains("does not parse"),
        "expected a parse-error message, got: {err}"
    );
}
