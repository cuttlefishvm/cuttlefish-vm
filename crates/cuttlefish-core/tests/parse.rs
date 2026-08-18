//! A spec grants capabilities, so what this parser *rejects* matters more than
//! what it accepts. Most of these tests are refusals: a spec that half-parses
//! would run a job under permissions nobody wrote down.

use cuttlefish_core::graph::{AcceptCheck, InputExpr, Rung};
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
    assert_eq!(spec.nodes.nodes.len(), 1);
    let (_, node) = &spec.nodes.nodes[0];
    assert_eq!(node.block, PathBuf::from("../blocks/echo-summarize"));
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
fn block_is_sugar_for_a_one_node_graph() {
    let spec = parse_spec(SAMPLE).unwrap();
    assert_eq!(spec.nodes.nodes.len(), 1);
    let (_, node) = &spec.nodes.nodes[0];
    assert_eq!(node.block, PathBuf::from("../blocks/echo-summarize"));
}

#[test]
fn a_nodes_block_with_fan_in_parses() {
    let src = SAMPLE.replace(
        r#"block = "../blocks/echo-summarize";"#,
        r#"nodes = {
             chunk = { block = "../blocks/chunk" };
             summarize = { block = "../blocks/summarize"; in = chunk.out; };
           };"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.nodes.nodes.len(), 2);
    assert_eq!(
        spec.nodes.get("summarize").unwrap().input,
        Some(InputExpr::FromNode("chunk".into()))
    );
}

#[test]
fn an_empty_nodes_block_is_rejected() {
    let src = SAMPLE.replace(r#"block = "../blocks/echo-summarize";"#, "nodes = { };");
    assert!(parse_spec(&src).is_err());
}

#[test]
fn pipeline_field_is_no_longer_recognized() {
    let src = SAMPLE.replace(
        r#"block = "../blocks/echo-summarize";"#,
        r#"pipeline = [ "../blocks/chunk", "../blocks/summarize" ];"#,
    );
    assert!(matches!(
        parse_spec(&src),
        Err(SpecError::UnknownField(f)) if f == "pipeline"
    ));
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

// --- Fan-out (`over`) ---------------------------------------------------
//
// A fan-out node runs its block once per manifest line instead of once. The
// manifest is read by the *host*, so the refusals below matter for the same
// reason every other refusal in this file does: a spec that half-parses runs
// a job touching paths nobody granted.

#[test]
fn a_node_can_declare_over_for_fan_out() {
    let spec = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "./corpus" ];
  nodes = { a = { block = "a@1"; over = "./corpus/manifest.jsonl"; }; };
}"#,
    )
    .expect("a node with `over` must parse");
    let node = spec.nodes.get("a").expect("node `a`");
    assert_eq!(
        node.over.as_deref(),
        Some(std::path::Path::new("./corpus/manifest.jsonl"))
    );
}

#[test]
fn over_and_repeat_until_on_one_node_is_rejected() {
    let err = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "./corpus" ];
  nodes = { a = { block = "a@1"; over = "./corpus/m.jsonl"; repeat_until = "done"; max_iterations = 3; }; };
}"#,
    )
    .expect_err("over + repeat_until must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("over"), "{msg}");
    assert!(msg.contains("repeat_until"), "{msg}");
}

#[test]
fn a_manifest_outside_every_read_root_is_rejected() {
    // The check now runs on resolved paths, so parsing succeeds and
    // `validate_host_read_paths` is what refuses. Both sides here are
    // relative and stay comparable, which is the case that always worked.
    let spec = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "./corpus" ];
  nodes = { a = { block = "a@1"; over = "./elsewhere/manifest.jsonl"; }; };
}"#,
    )
    .expect("this parses; the path check is a separate step");

    let err = spec
        .validate_host_read_paths()
        .expect_err("a manifest outside the read roots must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("elsewhere/manifest.jsonl"), "{msg}");
    assert!(msg.contains("capabilities"), "{msg}");
    // The granted roots are listed, because the failure is nearly always a
    // path that looks right and differs by a symlink or by being relative.
    assert!(
        msg.contains("./corpus"),
        "must name what was granted: {msg}"
    );
}

/// An absolute grant with a relative manifest is ordinary, and used to be
/// rejected outright.
///
/// This is the bug that surfaced running a real OCR pipeline: `capabilities
/// = [ Read "/tmp/work" ]` with `over = "./corpus/m.jsonl"` failed with
/// "outside every path granted", which reads as a permissions mistake. It is
/// not — lexically a relative path can never begin with an absolute one, so
/// *every* spec of this shape was refused no matter where the files were.
/// Resolving both against the spec's directory first is what makes the
/// question answerable at all.
#[test]
fn an_absolute_grant_covers_a_relative_manifest_once_both_are_resolved() {
    let dir = tempfile::tempdir().unwrap();
    // A backslash is a legal filename character on Unix, so this keeps the
    // Windows string-literal boundary covered on every CI platform.
    let root = dir.path().join(r"contains\C");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::create_dir_all(root.join("corpus")).unwrap();
    std::fs::write(root.join("corpus/m.jsonl"), "{}\n").unwrap();
    let root_literal = root
        .display()
        .to_string()
        .replace('\\', r"\\")
        .replace('"', r#"\""#);

    let mut spec = parse_spec(&format!(
        r#"spec s = {{
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "{}" ];
  nodes = {{ a = {{ block = "a@1"; over = "./corpus/m.jsonl"; }}; }};
}}"#,
        root_literal
    ))
    .expect("a spec of this shape must parse");

    // What cuttlefishd does before validating: resolve everything against
    // the spec's own directory, then canonicalize.
    spec.read_roots = spec
        .read_roots
        .iter()
        .map(|r| std::fs::canonicalize(root.join(r)).unwrap_or_else(|_| root.join(r)))
        .collect();
    for (_, node) in spec.nodes.nodes.iter_mut() {
        if let Some(m) = node.over.take() {
            node.over = Some(std::fs::canonicalize(root.join(&m)).unwrap_or(root.join(m)));
        }
    }

    spec.validate_host_read_paths()
        .expect("an absolute grant covering the manifest's real location must pass");
}

/// `./corpus` and `corpus` name the same directory. A naive
/// `Path::starts_with` says otherwise (a leading `./` is a real
/// `Component::CurDir`), which would reject perfectly ordinary specs.
#[test]
fn a_leading_dot_slash_does_not_change_whether_a_manifest_is_covered() {
    for (root, manifest) in [
        ("./corpus", "./corpus/m.jsonl"),
        ("corpus", "./corpus/m.jsonl"),
        ("./corpus", "corpus/m.jsonl"),
    ] {
        parse_spec(&format!(
            r#"spec s = {{
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "{root}" ];
  nodes = {{ a = {{ block = "a@1"; over = "{manifest}"; }}; }};
}}"#
        ))
        .unwrap_or_else(|e| panic!("Read {root:?} must cover {manifest:?}: {e}"));
    }
}

// --- Acceptance checks (`accept`) ---------------------------------------
//
// What "done" means beyond a node's declared type. Ordered and
// short-circuiting: Schema is deterministic and free, Judge costs a whole
// inference, so a schema-first list never pays for a judge on structurally
// broken output.

#[test]
fn a_node_can_declare_accept_checks() {
    let spec = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "./schemas" ];
  nodes = { a = { block = "a@1";
                  accept = [ Schema "./schemas/v.json", Judge "is it good?" ]; }; };
}"#,
    )
    .expect("accept must parse");
    let node = spec.nodes.get("a").unwrap();
    assert_eq!(node.accept.len(), 2);
    assert!(matches!(node.accept[0], AcceptCheck::Schema(_)));
    match &node.accept[1] {
        AcceptCheck::Judge { model, prompt } => {
            assert!(model.is_none(), "a bare Judge uses the spec's own model");
            assert_eq!(prompt, "is it good?");
        }
        other => panic!("expected a Judge, got {other:?}"),
    }
}

#[test]
fn a_judge_can_name_its_own_model() {
    // The form that actually works: a slow, strong model grading a fast
    // one's bulk output.
    let spec = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any; capabilities = [ ];
  nodes = { a = { block = "a@1";
                  accept = [ Judge { model = Ollama "llama3.3:70b"; prompt = "grade it"; } ]; }; };
}"#,
    )
    .expect("the record form of Judge must parse");
    match &spec.nodes.get("a").unwrap().accept[0] {
        AcceptCheck::Judge { model, prompt } => {
            assert_eq!(
                model.as_ref().unwrap(),
                &ModelRef::new("ollama", "llama3.3:70b")
            );
            assert_eq!(prompt, "grade it");
        }
        other => panic!("expected a Judge, got {other:?}"),
    }
}

#[test]
fn a_node_without_accept_has_no_checks() {
    // The default must stay exactly today's behaviour: the declared type is
    // the only contract.
    let spec = parse_spec(SAMPLE).unwrap();
    assert!(spec.nodes.get("block").unwrap().accept.is_empty());
}

// --- Recovery ladder (`on_fail`) ----------------------------------------

/// Build a one-node spec whose `on_fail` is `ladder`.
fn ladder_spec(ladder: &str) -> Result<cuttlefish_core::spec::Spec, SpecError> {
    parse_spec(&format!(
        r#"spec s = {{
  description = "d"; model = Stub "x"; data_policy = Any; capabilities = [ ];
  nodes = {{ a = {{ block = "a@1"; on_fail = {ladder}; }}; }};
}}"#
    ))
}

#[test]
fn a_node_can_declare_an_on_fail_ladder() {
    let spec = ladder_spec(r#"[ retry 2, reroute Ollama "llama3.3:70b", escalate ]"#)
        .expect("an on_fail ladder must parse");
    let rungs = &spec.nodes.get("a").unwrap().on_fail;
    assert_eq!(rungs.len(), 3);
    assert_eq!(rungs[0], Rung::Retry(2));
    assert_eq!(
        rungs[1],
        Rung::Reroute(ModelRef::new("ollama", "llama3.3:70b"))
    );
    assert_eq!(rungs[2], Rung::Escalate);
}

#[test]
fn escalate_must_be_the_last_rung() {
    // Anything after a terminal rung is unreachable. Accepting it would mean
    // a spec whose second half is decorative and whose author doesn't know.
    let err = ladder_spec("[ escalate, retry 2 ]").expect_err("must be rejected");
    assert!(err.to_string().contains("escalate"), "{err}");
}

#[test]
fn a_ladder_cannot_escalate_twice() {
    let err = ladder_spec("[ escalate, escalate ]").expect_err("must be rejected");
    assert!(err.to_string().contains("escalate"), "{err}");
}

#[test]
fn retry_zero_is_rejected() {
    // Expresses nothing: the author meant `retry 1`, or no rung at all.
    let err = ladder_spec("[ retry 0 ]").expect_err("must be rejected");
    assert!(err.to_string().contains("retry 0"), "{err}");
}

#[test]
fn a_node_without_on_fail_has_no_ladder() {
    let spec = parse_spec(SAMPLE).unwrap();
    assert!(spec.nodes.get("block").unwrap().on_fail.is_empty());
}

#[test]
fn a_schema_path_outside_every_read_root_is_rejected() {
    // Same move as the manifest check: parsing succeeds, and the path check
    // is its own step on resolved paths.
    let spec = parse_spec(
        r#"spec s = {
  description = "d"; model = Stub "x"; data_policy = Any;
  capabilities = [ Read "./corpus" ];
  nodes = { a = { block = "a@1"; accept = [ Schema "./elsewhere/v.json" ]; }; };
}"#,
    )
    .expect("this parses; the path check is a separate step");

    let err = spec
        .validate_host_read_paths()
        .expect_err("a schema outside the read roots must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("elsewhere/v.json"), "{msg}");
    assert!(msg.contains("capabilities"), "{msg}");
}

#[test]
fn a_fetch_grant_is_a_url_prefix_kept_exactly_as_written() {
    // The whole predictability of the grant rests on this: the string in the
    // spec is the string matched against. Normalising it here — adding a
    // trailing slash, lowercasing, resolving the host — would mean an author
    // cannot tell what they granted by reading their own capability line.
    let src = SAMPLE.replace(
        r#"[ Read "./docs" ]"#,
        r#"[ Read "./docs", Fetch "https://www.cms.gov/medicare/" ]"#,
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.read_roots, vec![PathBuf::from("./docs")]);
    assert_eq!(spec.fetch_prefixes, vec!["https://www.cms.gov/medicare/"]);
}

#[test]
fn a_fetch_grant_that_is_not_http_is_refused() {
    // `file://` through the fetch path would route around `Read` entirely,
    // which is the one thing a capability list exists to prevent.
    for prefix in ["file:///etc", "ftp://x.org/", "/etc/passwd"] {
        let src = SAMPLE.replace(r#"[ Read "./docs" ]"#, &format!(r#"[ Fetch "{prefix}" ]"#));
        assert!(
            parse_spec(&src).is_err(),
            "`Fetch \"{prefix}\"` must not parse"
        );
    }
}

#[test]
fn an_embedding_model_is_optional_and_separate_from_the_chat_model() {
    // Separate because they are different models: a chat model cannot embed,
    // and the failure when one is used for the other is a shaped-right,
    // meaningless vector rather than an error.
    assert!(parse_spec(SAMPLE).unwrap().embedding_model.is_none());

    let src = SAMPLE.replace(
        r#"  model = Path "./models/stub.gguf";"#,
        "  model = Path \"./models/stub.gguf\";\n  \
         embedding_model = Ollama \"nomic-embed-text\";",
    );
    let spec = parse_spec(&src).unwrap();
    assert_eq!(spec.model, ModelRef::new("path", "./models/stub.gguf"));
    assert_eq!(
        spec.embedding_model,
        Some(ModelRef::new("ollama", "nomic-embed-text"))
    );
}
