//! A spec grants capabilities, so what this parser *rejects* matters more than
//! what it accepts. Most of these tests are refusals: a spec that half-parses
//! would run a job under permissions nobody wrote down.

use cuttlefish_core::spec::{parse_spec, DataPolicy, ModelRef, SpecError};
use std::path::PathBuf;

const SAMPLE: &str = r#"
spec summarize_docs = {
  description = "Use when the agent needs a summary of a local file.";
  model = Path "./models/stub.gguf";
  data_policy = Local_only;
  capabilities = [ Read "./docs" ];
  block = "../blocks/echo-summarize";
}
"#;

#[test]
fn parses_every_field_of_the_sample() {
    let spec = parse_spec(SAMPLE).expect("the sample must parse");

    assert_eq!(spec.name, "summarize_docs");
    assert!(spec.description.starts_with("Use when"));
    assert_eq!(spec.model, ModelRef::Path("./models/stub.gguf".into()));
    assert_eq!(spec.data_policy, DataPolicy::LocalOnly);
    assert_eq!(spec.read_roots, vec![PathBuf::from("./docs")]);
    assert_eq!(spec.block, PathBuf::from("../blocks/echo-summarize"));
}

#[test]
fn accepts_several_capabilities() {
    let src = SAMPLE.replace(
        r#"[ Read "./docs" ]"#,
        r#"[ Read "./docs", Read "./notes" ]"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(
        spec.read_roots,
        vec![PathBuf::from("./docs"), PathBuf::from("./notes")]
    );
}

#[test]
fn accepts_an_empty_capability_list() {
    // Granting nothing is meaningful and must not be confused with granting
    // everything — a spec that reads no files is a legitimate spec.
    let src = SAMPLE.replace(r#"[ Read "./docs" ]"#, "[ ]");
    let spec = parse_spec(&src).unwrap();
    assert!(spec.read_roots.is_empty());
}

#[test]
fn rejects_a_missing_required_field() {
    let src = SAMPLE.replace(r#"  block = "../blocks/echo-summarize";"#, "");
    let err = parse_spec(&src).unwrap_err();
    assert!(
        matches!(err, SpecError::MissingField("block")),
        "the error must name the missing field, got: {err}"
    );
}

#[test]
fn rejects_an_unknown_field_rather_than_ignoring_it() {
    // Silently skipping an unrecognised key is how a typo'd `capabilities`
    // becomes a spec with no capabilities that still runs.
    let src = SAMPLE.replace("  block =", "  frobnicate = 3;\n  block =");
    let err = parse_spec(&src).unwrap_err();
    assert!(
        matches!(&err, SpecError::UnknownField(f) if f == "frobnicate"),
        "the error must name the unknown field, got: {err}"
    );
}

#[test]
fn rejects_a_misspelled_capability_kind() {
    let src = SAMPLE.replace(r#"Read "./docs""#, r#"Write "./docs""#);
    let err = parse_spec(&src).unwrap_err();
    assert!(
        err.to_string().contains("Write"),
        "the unsupported capability must be named, got: {err}"
    );
}

#[test]
fn rejects_an_unsupported_model_kind() {
    // `Hf` is a planned model source, not a supported one. Accepting it and
    // ignoring the difference would silently resolve the wrong model.
    let src = SAMPLE.replace(r#"Path "./models/stub.gguf""#, r#"Hf "org/model#q4""#);
    let err = parse_spec(&src).unwrap_err();
    assert!(
        matches!(&err, SpecError::UnsupportedModel(k) if k == "Hf"),
        "got: {err}"
    );
}

#[test]
fn rejects_an_unknown_data_policy() {
    let src = SAMPLE.replace("data_policy = Local_only;", "data_policy = Whatever;");
    let err = parse_spec(&src).unwrap_err();
    assert!(err.to_string().contains("Whatever"), "got: {err}");
}

#[test]
fn rejects_an_unquoted_string_field() {
    let src = SAMPLE.replace(r#"block = "../blocks/echo-summarize""#, "block = bare_word");
    assert!(parse_spec(&src).is_err());
}

#[test]
fn rejects_a_capability_list_that_is_not_a_list() {
    let src = SAMPLE.replace(
        r#"capabilities = [ Read "./docs" ]"#,
        r#"capabilities = "docs""#,
    );
    assert!(parse_spec(&src).is_err());
}

#[test]
fn rejects_input_with_no_spec_block() {
    assert!(parse_spec("").is_err());
    assert!(parse_spec("spec foo =").is_err());
    assert!(parse_spec("just some prose").is_err());
}

#[test]
fn rejects_a_spec_with_no_name() {
    let src = SAMPLE.replace("spec summarize_docs =", "spec =");
    assert!(parse_spec(&src).is_err());
}

#[test]
fn tolerates_extra_whitespace_and_a_trailing_semicolon() {
    let src = r#"
        spec   tidy   =   {
            description = "d" ;
            model = Path "m" ;
            data_policy = Any ;
            capabilities = [ ] ;
            block = "b" ;
        }
    "#;
    let spec = parse_spec(src).unwrap();
    assert_eq!(spec.name, "tidy");
    assert_eq!(spec.data_policy, DataPolicy::Any);
}

#[test]
fn the_repository_example_spec_parses() {
    // Guards against the shipped example drifting away from the parser, which
    // would otherwise only be noticed by someone following the README.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/summarize.cuttlefish");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    let spec = parse_spec(&src).expect("the shipped example must parse");
    assert_eq!(spec.name, "summarize_docs");
    assert_eq!(spec.data_policy, DataPolicy::LocalOnly);
}
