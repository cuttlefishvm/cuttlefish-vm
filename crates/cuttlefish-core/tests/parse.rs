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
    assert_eq!(spec.model, ModelRef::new("path", "./models/stub.gguf"));
    assert_eq!(spec.data_policy, DataPolicy::LocalOnly);
    assert_eq!(spec.read_roots, vec![PathBuf::from("./docs")]);
    assert_eq!(spec.block(), PathBuf::from("../blocks/echo-summarize"));
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
fn any_provider_name_parses() {
    // The parser deliberately does not know which providers exist — inference
    // can come from Ollama, an HTTP endpoint, an embedded runtime, or something
    // not written yet. Whether a provider is available is the host's question,
    // answered when the model is resolved, so a spec naming an unknown one is
    // syntactically fine and fails later with a list of what *is* registered.
    let src = SAMPLE.replace(r#"Path "./models/stub.gguf""#, r#"SomethingNew "whatever""#);
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.model, ModelRef::new("somethingnew", "whatever"));
}

#[test]
fn provider_names_are_case_insensitive() {
    // `Ollama`, `ollama`, and `OLLAMA` must name the same provider; a spec
    // should not fail over capitalisation.
    for spelling in ["Ollama", "ollama", "OLLAMA"] {
        let src = SAMPLE.replace(
            r#"Path "./models/stub.gguf""#,
            &format!(r#"{spelling} "llama3.2:1b""#),
        );
        let spec = parse_spec(&src).unwrap();
        assert_eq!(spec.model.provider, "ollama", "for spelling {spelling}");
    }
}

#[test]
fn rejects_a_provider_name_that_is_not_an_identifier() {
    let src = SAMPLE.replace(r#"Path "./models/stub.gguf""#, r#""quoted" "target""#);
    assert!(parse_spec(&src).is_err());
}

#[test]
fn rejects_a_model_with_no_target() {
    let src = SAMPLE.replace(r#"model = Path "./models/stub.gguf""#, "model = Ollama");
    assert!(parse_spec(&src).is_err());
}

#[test]
fn parses_an_ollama_model_reference() {
    let src = SAMPLE.replace(r#"Path "./models/stub.gguf""#, r#"Ollama "llama3.2:1b""#);
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.model, ModelRef::new("ollama", "llama3.2:1b"));
}

#[test]
fn an_ollama_tag_keeps_its_colon() {
    // Ollama model names carry a `:tag` suffix. If the parser ever splits on
    // punctuation, `llama3.2:1b` silently becomes `llama3.2` — a different
    // model that may well exist locally, so nothing would visibly break.
    let src = SAMPLE.replace(
        r#"Path "./models/stub.gguf""#,
        r#"Ollama "qwen2.5:7b-instruct-q4_K_M""#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(
        spec.model,
        ModelRef::new("ollama", "qwen2.5:7b-instruct-q4_K_M")
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

#[test]
fn a_pipeline_of_several_blocks_parses_in_order() {
    // Order is the whole meaning of a pipeline; reversing it would typecheck
    // differently and run differently.
    let src = SAMPLE.replace(
        r#"block = "../blocks/echo-summarize";"#,
        r#"pipeline = [ "../blocks/chunk", "../blocks/summarize" ];"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(
        spec.pipeline,
        vec![
            PathBuf::from("../blocks/chunk"),
            PathBuf::from("../blocks/summarize")
        ]
    );
}

#[test]
fn block_is_sugar_for_a_one_element_pipeline() {
    // Both spellings must produce the same thing, so nothing downstream has to
    // handle two cases.
    let spec = parse_spec(SAMPLE).unwrap();
    assert_eq!(spec.pipeline.len(), 1);
    assert_eq!(spec.block(), spec.pipeline[0]);
}

#[test]
fn an_empty_pipeline_is_rejected() {
    // A pipeline that runs nothing and returns nothing is never what was meant.
    let src = SAMPLE.replace(r#"block = "../blocks/echo-summarize";"#, "pipeline = [ ];");
    assert!(parse_spec(&src).is_err());
}

// -- things the old character-splitting parser got wrong --------------------

#[test]
fn a_semicolon_inside_a_description_is_just_text() {
    // The bug that motivated a real lexer. Descriptions are prose, so they
    // contain semicolons routinely; splitting statements on `;` cut this in
    // half and blamed the description for not being a quoted string.
    let src = SAMPLE.replace(
        r#"description = "Use when the agent needs a summary of a local file.";"#,
        r#"description = "Use when summarizing; especially long files.";"#,
    );
    let spec = parse_spec(&src).expect("a semicolon in prose must parse");
    assert_eq!(
        spec.description,
        "Use when summarizing; especially long files."
    );
}

#[test]
fn a_comma_inside_a_path_is_just_text() {
    // Same failure, different separator: capability lists split on `,`.
    let src = SAMPLE.replace(r#"Read "./docs""#, r#"Read "./my,docs""#);
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.read_roots, vec![PathBuf::from("./my,docs")]);
}

#[test]
fn braces_and_brackets_inside_strings_do_not_confuse_the_parser() {
    let src = SAMPLE.replace(
        r#"description = "Use when the agent needs a summary of a local file.";"#,
        r#"description = "Use when the input looks like { a: [1] }.";"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert!(
        spec.description.contains("{ a: [1] }"),
        "{}",
        spec.description
    );
}

#[test]
fn escapes_are_resolved() {
    let src = SAMPLE.replace(
        r#"description = "Use when the agent needs a summary of a local file.";"#,
        r#"description = "Say \"hello\"\nthen stop.";"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.description, "Say \"hello\"\nthen stop.");
}

#[test]
fn comments_are_ignored() {
    let src = format!("# what this job is for\n{SAMPLE}\n# trailing note\n");
    let spec = parse_spec(&src).expect("comments must not break parsing");
    assert_eq!(spec.name, "summarize_docs");
}

#[test]
fn a_trailing_semicolon_is_optional() {
    let src = SAMPLE.replace(
        r#"block = "../blocks/echo-summarize";"#,
        r#"block = "../blocks/echo-summarize""#,
    );
    assert!(parse_spec(&src).is_ok());
}

#[test]
fn an_unterminated_string_is_reported_with_a_position() {
    // "malformed spec" with no location is a poor error for a hand-edited file.
    let src = SAMPLE.replace(
        r#""../blocks/echo-summarize";"#,
        r#""../blocks/echo-summarize;"#,
    );
    let err = parse_spec(&src).unwrap_err().to_string();
    assert!(err.contains("unterminated"), "{err}");
    assert!(err.contains("line"), "the error should say where: {err}");
}

#[test]
fn a_syntax_error_says_what_was_expected_and_where() {
    let src = SAMPLE.replace("data_policy = Local_only;", "data_policy Local_only;");
    let err = parse_spec(&src).unwrap_err().to_string();
    assert!(err.contains("expected `=`"), "{err}");
    assert!(err.contains("line"), "{err}");
}

#[test]
fn an_unknown_escape_is_rejected_rather_than_silently_kept() {
    let src = SAMPLE.replace(
        r#"description = "Use when the agent needs a summary of a local file.";"#,
        r#"description = "a \q b";"#,
    );
    assert!(parse_spec(&src).is_err());
}
