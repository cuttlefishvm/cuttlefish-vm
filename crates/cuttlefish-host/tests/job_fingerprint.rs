use cuttlefish_abi::Ty;
use cuttlefish_core::spec::{parse_spec, ModelRef};
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::{graph_fingerprint, job_fingerprint, CheckedNode};

fn node() -> CheckedNode {
    CheckedNode {
        name: "candidate".into(),
        kind: ArtifactKind::Script,
        resolved: None,
        module_bytes: vec![1, 2, 3],
        signature: cuttlefish_abi::Signature {
            input: Ty::Json,
            output: Ty::Json,
        },
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: Some("input".into()),
        over: None,
        item_output: None,
        accept: vec![],
        on_fail: vec![],
    }
}

#[test]
fn configured_generation_pins_options_executable_bytes_wiring_and_schema() {
    let mut spec = parse_spec(include_str!(
        "../../../examples/synthetic-candidates/pipeline.cuttlefish"
    ))
    .unwrap();
    spec.warehouse_required = false;
    spec.model = ModelRef::new(
        "ollama",
        r#"{"model":"test:1","expected_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","seed":42}"#,
    );
    let nodes = vec![node()];
    let expected = job_fingerprint(&nodes, &spec).unwrap();
    assert_eq!(expected, job_fingerprint(&nodes, &spec).unwrap());
    let mut changed = spec.clone();
    changed.model.target = changed.model.target.replace("42", "43");
    assert_ne!(expected, job_fingerprint(&nodes, &changed).unwrap());
    let mut changed = spec.clone();
    changed.model.target = changed
        .model
        .target
        .replace(&"a".repeat(64), &"b".repeat(64));
    assert_ne!(expected, job_fingerprint(&nodes, &changed).unwrap());
    let mut changed = nodes.clone();
    changed[0].script = Some("infer(\"changed prompt\",1)".into());
    assert_ne!(expected, job_fingerprint(&changed, &spec).unwrap());
    let mut changed = nodes.clone();
    changed[0].module_bytes.push(4);
    assert_ne!(expected, job_fingerprint(&changed, &spec).unwrap());
    let mut changed = nodes.clone();
    changed[0].input = Some(cuttlefish_core::graph::InputExpr::FromNode("other".into()));
    assert_ne!(expected, job_fingerprint(&changed, &spec).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let schema = dir.path().join("accept.json");
    std::fs::write(&schema, r#"{"type":"string"}"#).unwrap();
    let mut changed = nodes.clone();
    changed[0].accept = vec![cuttlefish_core::graph::AcceptCheck::Schema(schema.clone())];
    let before = job_fingerprint(&changed, &spec).unwrap();
    std::fs::write(&schema, r#"{"type":"number"}"#).unwrap();
    assert_ne!(before, job_fingerprint(&changed, &spec).unwrap());
    std::fs::remove_file(schema).unwrap();
    assert!(job_fingerprint(&changed, &spec).is_err());
}

#[test]
fn legacy_jobs_keep_their_old_fingerprint_and_required_publication_opts_in() {
    let mut spec = parse_spec(include_str!(
        "../../../examples/synthetic-candidates/pipeline.cuttlefish"
    ))
    .unwrap();
    let nodes = vec![node()];
    spec.warehouse_required = false;
    assert_eq!(
        graph_fingerprint(&nodes),
        job_fingerprint(&nodes, &spec).unwrap()
    );
    spec.warehouse_required = true;
    assert_ne!(
        graph_fingerprint(&nodes),
        job_fingerprint(&nodes, &spec).unwrap()
    );
}
