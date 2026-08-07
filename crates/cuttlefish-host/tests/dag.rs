//! Tests for graph typechecking — the DAG-shaped analogue of
//! `crates/cuttlefish-host/tests/pipeline.rs`.
//!
//! `NodeGraph`/`Branches` are built directly in Rust rather than parsed from
//! source text: it keeps each test's shape (which node feeds which, which
//! labels apply) visible right where the assertions are, instead of buried
//! in a source string the reader has to mentally re-parse.

mod support;

use cuttlefish_core::graph::{Branches, InputExpr, Node, NodeGraph};
use cuttlefish_host::catalog::ArtifactKind;
use cuttlefish_host::dag::{
    check_graph, graph_fingerprint, BranchExclusivity, CheckedNode, DagError,
};
use cuttlefish_host::pipeline::ResolvedInput;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use support::block_with;
use wasmtime::Engine;

/// Compile a real block and load it into a `ResolvedInput`, keyed by node
/// name — real bytes, same reasoning `pipeline.rs`'s `direct()` gives: a
/// mocked signature would test the checker against a fiction, not the
/// artifact that will actually run.
fn resolved_for(dir: &Path, name: &str, input: &str, output: &str) -> ResolvedInput {
    let path = block_with(dir, name, input, output);
    ResolvedInput {
        name: name.to_string(),
        kind: ArtifactKind::Block,
        resolved: None,
        bytes: std::fs::read(&path).unwrap(),
        script: None,
    }
}

/// A node with no repeat_until/max_iterations — the common case.
fn node(input: Option<InputExpr>) -> Node {
    Node {
        block: PathBuf::new(),
        input,
        repeat_until: None,
        max_iterations: None,
    }
}

fn from(name: &str) -> InputExpr {
    InputExpr::FromNode(name.to_string())
}

fn record(fields: &[(&str, InputExpr)]) -> InputExpr {
    let mut map = BTreeMap::new();
    for (k, v) in fields {
        map.insert(k.to_string(), v.clone());
    }
    InputExpr::Record(map)
}

#[test]
fn a_linear_two_node_graph_typechecks() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "chunk".to_string(),
        resolved_for(dir.path(), "chunk", "{path: text}", "[text]"),
    );
    resolved.insert(
        "summarize".to_string(),
        resolved_for(dir.path(), "summarize", "[text]", "{summary: text}"),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("chunk".to_string(), node(None)),
            ("summarize".to_string(), node(Some(from("chunk")))),
        ],
    };

    let checked = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .expect("a linear two-node graph with matching seams must typecheck");

    assert_eq!(checked.nodes.len(), 2);
    assert_eq!(
        checked
            .nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        vec!["chunk", "summarize"],
        "topological order must run producer before consumer"
    );
}

#[test]
fn a_checked_script_node_carries_its_script_text_through() {
    let script_text = "//! signature: {n: json} -> {n: json}\ninput\n";
    let mut resolved = HashMap::new();
    resolved.insert(
        "echo".to_string(),
        ResolvedInput {
            name: "echo".to_string(),
            kind: ArtifactKind::Script,
            resolved: Some("echo@1".to_string()),
            bytes: cuttlefish_host::embedded_rhai_interpreter_bytes().to_vec(),
            script: Some(script_text.to_string()),
        },
    );

    let graph = NodeGraph {
        nodes: vec![("echo".to_string(), node(None))],
    };

    let checked = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .expect("a single Script node with a valid signature header must typecheck");

    assert_eq!(checked.nodes.len(), 1);
    assert_eq!(checked.nodes[0].kind, ArtifactKind::Script);
    assert_eq!(checked.nodes[0].script.as_deref(), Some(script_text));
}

#[test]
fn a_checked_block_node_has_no_script_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "block".to_string(),
        resolved_for(dir.path(), "block", "json", "json"),
    );

    let graph = NodeGraph {
        nodes: vec![("block".to_string(), node(None))],
    };

    let checked = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .expect("a single Block node must typecheck");

    assert_eq!(checked.nodes.len(), 1);
    assert_eq!(checked.nodes[0].kind, ArtifactKind::Block);
    assert!(checked.nodes[0].script.is_none());
}

#[test]
fn fan_in_via_record_typechecks_when_seams_match() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "docs".to_string(),
        resolved_for(dir.path(), "docs", "{}", "[text]"),
    );
    resolved.insert(
        "images".to_string(),
        resolved_for(dir.path(), "images", "{}", "[image]"),
    );
    resolved.insert(
        "report".to_string(),
        resolved_for(
            dir.path(),
            "report",
            "{docs: [text], images: [image]}",
            "{summary: text}",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("docs".to_string(), node(None)),
            ("images".to_string(), node(None)),
            (
                "report".to_string(),
                node(Some(record(&[
                    ("docs", from("docs")),
                    ("images", from("images")),
                ]))),
            ),
        ],
    };

    let checked = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .expect("fan-in whose record shape matches the consumer's declared input must typecheck");
    assert_eq!(checked.nodes.len(), 3);
}

#[test]
fn fan_in_via_record_fails_on_a_missing_field() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "docs".to_string(),
        resolved_for(dir.path(), "docs2", "{}", "[text]"),
    );
    resolved.insert(
        "images".to_string(),
        resolved_for(dir.path(), "images2", "{}", "[image]"),
    );
    // report needs an `extra` field that neither producer supplies.
    resolved.insert(
        "report".to_string(),
        resolved_for(
            dir.path(),
            "report2",
            "{docs: [text], images: [image], extra: text}",
            "{summary: text}",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("docs".to_string(), node(None)),
            ("images".to_string(), node(None)),
            (
                "report".to_string(),
                node(Some(record(&[
                    ("docs", from("docs")),
                    ("images", from("images")),
                ]))),
            ),
        ],
    };

    let err = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .err()
        .expect("a record missing a required field must be rejected");
    assert!(matches!(err, DagError::SeamMismatch { .. }), "{err:?}");
}

/// The exact mistake a real session made: `analyst` produces
/// `{segment, finding, risk}`, `stress` declares that same shape as its
/// input, and the fix is the bare pass-through `in = analyst.out;` -- but
/// it's natural to instead wrap it field by field, `{ segment =
/// analyst.out; finding = analyst.out; risk = analyst.out; }`, which nests
/// analyst's whole output under every field instead of using its fields
/// directly. That's still rejected (this isn't a correctness bug — the
/// resulting nested type genuinely doesn't match `stress`'s declared
/// input), but the message should name the fix rather than just dump the
/// mismatched types.
#[test]
fn wrapping_one_node_s_output_across_every_field_names_the_bare_passthrough_fix() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "analyst".to_string(),
        resolved_for(
            dir.path(),
            "analyst",
            "{facts: text}",
            "{segment: text, finding: text, risk: text}",
        ),
    );
    resolved.insert(
        "stress".to_string(),
        resolved_for(
            dir.path(),
            "stress",
            "{segment: text, finding: text, risk: text}",
            "{failure_mode: text}",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("analyst".to_string(), node(None)),
            (
                "stress".to_string(),
                node(Some(record(&[
                    ("segment", from("analyst")),
                    ("finding", from("analyst")),
                    ("risk", from("analyst")),
                ]))),
            ),
        ],
    };

    let err = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .err()
        .expect("wrapping one node's output field by field must still be rejected");
    let message = err.to_string();
    assert!(
        message.contains("in = analyst.out;"),
        "expected the bare-passthrough hint in: {message}"
    );
}

#[test]
fn an_undeclared_cycle_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "a".to_string(),
        resolved_for(dir.path(), "cyc_a", "text", "text"),
    );
    resolved.insert(
        "b".to_string(),
        resolved_for(dir.path(), "cyc_b", "text", "text"),
    );

    // a depends on b, b depends on a — neither declares repeat_until.
    let graph = NodeGraph {
        nodes: vec![
            ("a".to_string(), node(Some(from("b")))),
            ("b".to_string(), node(Some(from("a")))),
        ],
    };

    let err = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .err()
        .expect("a two-node cycle with no repeat_until marker must be rejected");
    assert!(matches!(err, DagError::UnmarkedCycle { .. }), "{err:?}");
}

#[test]
fn a_bare_self_reference_without_repeat_until_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "node".to_string(),
        resolved_for(dir.path(), "bare_self", "text", "text"),
    );

    // Self-reference with NO repeat_until declared at all — this must be
    // caught the same way an undeclared two-node cycle is: repeat_until's
    // presence is the only thing that turns a self-edge into a legal loop.
    let graph = NodeGraph {
        nodes: vec![("node".to_string(), node(Some(from("node"))))],
    };
    // Sanity: confirm the fixture really is self-referencing and really has
    // no repeat_until, so this test can't pass for the wrong reason.
    match &graph.nodes[0].1.input {
        Some(InputExpr::FromNode(target)) => assert_eq!(target, "node"),
        other => panic!("expected a bare self-reference, got {other:?}"),
    }
    assert!(graph.nodes[0].1.repeat_until.is_none());

    let err = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .err()
        .expect("a bare self-reference without repeat_until must be rejected");
    assert!(matches!(err, DagError::UnmarkedCycle { .. }), "{err:?}");
}

#[test]
fn a_repeat_until_self_reference_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "refine".to_string(),
        resolved_for(dir.path(), "refine", "text", "text"),
    );

    let graph = NodeGraph {
        nodes: vec![(
            "refine".to_string(),
            Node {
                block: PathBuf::new(),
                input: Some(from("refine")),
                repeat_until: Some("done".to_string()),
                max_iterations: Some(5),
            },
        )],
    };

    let checked = check_graph(&Engine::default(), &graph, &Branches::default(), &resolved)
        .expect("a self-reference marked repeat_until with max_iterations must be accepted");
    assert_eq!(checked.nodes.len(), 1);
    assert_eq!(checked.nodes[0].name, "refine");
    assert_eq!(checked.nodes[0].repeat_until.as_deref(), Some("done"));
    assert_eq!(checked.nodes[0].max_iterations, Some(5));
}

#[test]
fn mixed_branch_fan_in_is_a_build_time_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "classify".to_string(),
        resolved_for(dir.path(), "classify_a", "text", "{route: text}"),
    );
    resolved.insert(
        "handle_pdf".to_string(),
        resolved_for(
            dir.path(),
            "handle_pdf_a",
            "{route: text}",
            "{result: text}",
        ),
    );
    resolved.insert(
        "handle_scan".to_string(),
        resolved_for(
            dir.path(),
            "handle_scan_a",
            "{route: text}",
            "{result: text}",
        ),
    );
    resolved.insert(
        "joined".to_string(),
        resolved_for(
            dir.path(),
            "joined_a",
            "{from_pdf: {result: text}, from_scan: {result: text}}",
            "text",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("classify".to_string(), node(None)),
            ("handle_pdf".to_string(), node(Some(from("classify")))),
            ("handle_scan".to_string(), node(Some(from("classify")))),
            (
                "joined".to_string(),
                node(Some(record(&[
                    ("from_pdf", from("handle_pdf")),
                    ("from_scan", from("handle_scan")),
                ]))),
            ),
        ],
    };
    let branches = Branches {
        decisions: vec![(
            "classify".to_string(),
            vec![
                ("pdf".to_string(), "handle_pdf".to_string()),
                ("scan".to_string(), "handle_scan".to_string()),
            ],
        )],
    };

    let err = check_graph(&Engine::default(), &graph, &branches, &resolved)
        .err()
        .expect("a node fed by two mutually-exclusive branch outcomes must be rejected");
    match err {
        DagError::ConflictingBranchFanIn {
            node,
            decision,
            label_a,
            label_b,
        } => {
            assert_eq!(node, "joined");
            assert_eq!(decision, "classify");
            let labels = [label_a, label_b];
            assert!(labels.contains(&"pdf".to_string()));
            assert!(labels.contains(&"scan".to_string()));
        }
        other => panic!("expected ConflictingBranchFanIn, got {other:?}"),
    }
}

#[test]
fn a_node_combining_one_branch_output_with_one_unconditional_output_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "classify".to_string(),
        resolved_for(dir.path(), "classify_b", "text", "{route: text}"),
    );
    resolved.insert(
        "handle_pdf".to_string(),
        resolved_for(
            dir.path(),
            "handle_pdf_b",
            "{route: text}",
            "{result: text}",
        ),
    );
    // `other` is unrelated to the classify decision entirely — an
    // independent entry point.
    resolved.insert(
        "other".to_string(),
        resolved_for(dir.path(), "other_b", "text", "{extra: text}"),
    );
    resolved.insert(
        "joined".to_string(),
        resolved_for(
            dir.path(),
            "joined_b",
            "{from_route: {result: text}, extra: {extra: text}}",
            "text",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("classify".to_string(), node(None)),
            ("handle_pdf".to_string(), node(Some(from("classify")))),
            ("other".to_string(), node(None)),
            (
                "joined".to_string(),
                node(Some(record(&[
                    ("from_route", from("handle_pdf")),
                    ("extra", from("other")),
                ]))),
            ),
        ],
    };
    let branches = Branches {
        decisions: vec![(
            "classify".to_string(),
            vec![("pdf".to_string(), "handle_pdf".to_string())],
        )],
    };

    let checked = check_graph(&Engine::default(), &graph, &branches, &resolved).expect(
        "one branch-exclusive input plus one unconditional input on the same node must be fine",
    );

    assert_eq!(
        checked.exclusive_to.get("joined"),
        Some(&BranchExclusivity {
            decision: "classify".to_string(),
            label: "pdf".to_string(),
        }),
        "joined must be recorded as exclusive to classify's pdf label, transitively via handle_pdf"
    );
}

#[test]
fn two_different_decisions_labeling_the_same_node_is_not_a_conflict() {
    // A node fed by one label from decision "classify" and one label from
    // an unrelated decision "urgency" — genuinely fine, since they're
    // independent decisions, not two mutually-exclusive outcomes of the
    // same one. This is exactly the case compute_branch_exclusivity's
    // `same_decision` guard exists to permit: without that guard (i.e. if
    // any two differing labels were treated as a conflict regardless of
    // which decision they belong to), this would incorrectly fail with
    // ConflictingBranchFanIn.
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "classify".to_string(),
        resolved_for(dir.path(), "classify_c", "{}", "{route: text}"),
    );
    resolved.insert(
        "urgency".to_string(),
        resolved_for(dir.path(), "urgency_c", "{}", "{level: text}"),
    );
    resolved.insert(
        "handle_pdf".to_string(),
        resolved_for(
            dir.path(),
            "handle_pdf_c",
            "{route: text}",
            "{result: text}",
        ),
    );
    resolved.insert(
        "handle_high".to_string(),
        resolved_for(
            dir.path(),
            "handle_high_c",
            "{level: text}",
            "{result: text}",
        ),
    );
    // handle_scan and handle_low are the *other* label of each decision —
    // never referenced by any other node's InputExpr, but a branch target
    // must now name a real graph node (that's the target-validation fix
    // this suite also covers), so each gets a trivial unconditional stub
    // purely to exist.
    resolved.insert(
        "handle_scan".to_string(),
        resolved_for(dir.path(), "handle_scan_c", "{}", "{}"),
    );
    resolved.insert(
        "handle_low".to_string(),
        resolved_for(dir.path(), "handle_low_c", "{}", "{}"),
    );
    resolved.insert(
        "joined".to_string(),
        resolved_for(
            dir.path(),
            "joined_c",
            "{a: {result: text}, b: {result: text}}",
            "text",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("classify".to_string(), node(None)),
            ("urgency".to_string(), node(None)),
            ("handle_pdf".to_string(), node(Some(from("classify")))),
            ("handle_high".to_string(), node(Some(from("urgency")))),
            ("handle_scan".to_string(), node(None)),
            ("handle_low".to_string(), node(None)),
            (
                "joined".to_string(),
                node(Some(record(&[
                    ("a", from("handle_pdf")),
                    ("b", from("handle_high")),
                ]))),
            ),
        ],
    };
    // Two independent decisions, each with two labels, so each genuinely has
    // more than one mutually-exclusive outcome — only one target per
    // decision actually feeds `joined`; the other exists solely so "same
    // decision" has real meaning to check against (and, as a real graph
    // node, satisfies branch-target validation).
    let branches = Branches {
        decisions: vec![
            (
                "classify".to_string(),
                vec![
                    ("pdf".to_string(), "handle_pdf".to_string()),
                    ("scan".to_string(), "handle_scan".to_string()),
                ],
            ),
            (
                "urgency".to_string(),
                vec![
                    ("high".to_string(), "handle_high".to_string()),
                    ("low".to_string(), "handle_low".to_string()),
                ],
            ),
        ],
    };

    let checked = check_graph(&Engine::default(), &graph, &branches, &resolved).expect(
        "a node fed by one label each from two different decisions must not be treated as a conflict",
    );
    assert_eq!(checked.nodes.len(), 7);
    // The point of this test is that check_graph returns Ok — which of
    // "pdf"/"high" ends up as joined's exclusive_to value is an unspecified
    // tie-break of propagation order, not something this test should pin
    // down.
}

#[test]
fn an_unknown_branch_target_is_rejected() {
    // branches.classify's "pdf" label points at "handle_pdf_TYPO", which was
    // never added as a node anywhere in the graph — the kind of typo that,
    // without target validation, would silently leave the *real* node
    // (handle_pdf) ungated and running unconditionally at execution time.
    // This must be caught here, at build time, instead.
    let dir = tempfile::tempdir().unwrap();
    let mut resolved = HashMap::new();
    resolved.insert(
        "classify".to_string(),
        resolved_for(dir.path(), "classify_typo", "text", "{route: text}"),
    );
    resolved.insert(
        "handle_pdf".to_string(),
        resolved_for(
            dir.path(),
            "handle_pdf_typo",
            "{route: text}",
            "{result: text}",
        ),
    );

    let graph = NodeGraph {
        nodes: vec![
            ("classify".to_string(), node(None)),
            ("handle_pdf".to_string(), node(Some(from("classify")))),
        ],
    };
    let branches = Branches {
        decisions: vec![(
            "classify".to_string(),
            vec![("pdf".to_string(), "handle_pdf_TYPO".to_string())],
        )],
    };

    let err = check_graph(&Engine::default(), &graph, &branches, &resolved)
        .err()
        .expect("a branch target that names no real node must be rejected");
    match err {
        DagError::UnknownReference { node, referenced } => {
            assert_eq!(node, "classify", "must name the branching decision");
            assert_eq!(
                referenced, "handle_pdf_TYPO",
                "must name the bad target, not the real node it was meant to gate"
            );
        }
        other => panic!("expected UnknownReference, got {other:?}"),
    }
}

/// A minimal `CheckedNode` with a given name and signature — the two fields
/// `graph_fingerprint` actually reads. Everything else is filled with a
/// plain, unconditional shape, same convention as `runner.rs` tests' own
/// `node()` helper.
fn checked_node(name: &str, signature: cuttlefish_abi::Signature) -> CheckedNode {
    CheckedNode {
        name: name.to_string(),
        kind: ArtifactKind::Block,
        resolved: None,
        module_bytes: Vec::new(),
        signature,
        input: None,
        repeat_until: None,
        max_iterations: None,
        script: None,
    }
}

fn sig(input: &str, output: &str) -> cuttlefish_abi::Signature {
    cuttlefish_abi::Signature {
        input: input.parse().expect("valid Ty text"),
        output: output.parse().expect("valid Ty text"),
    }
}

#[test]
fn graph_fingerprint_differs_when_a_node_signature_changes() {
    let a = vec![checked_node("chunk", sig("{path: text}", "[text]"))];
    let b = vec![checked_node(
        "chunk",
        sig("{path: text}", "{summary: text}"),
    )];

    assert_ne!(
        graph_fingerprint(&a),
        graph_fingerprint(&b),
        "changing a node's declared signature must change the fingerprint"
    );
}

#[test]
fn graph_fingerprint_is_stable_for_the_same_graph() {
    let nodes = vec![
        checked_node("chunk", sig("{path: text}", "[text]")),
        checked_node("summarize", sig("[text]", "{summary: text}")),
    ];

    assert_eq!(
        graph_fingerprint(&nodes),
        graph_fingerprint(&nodes),
        "fingerprinting the same graph twice must produce the same string"
    );
}

/// Regression test for the old `\0`-delimiter collision in
/// `graph_fingerprint`: two structurally different node sequences that
/// concatenate to the *exact same byte string* under the old
/// `name + "\0" + signature.to_string() + "\0"` join, because
/// `Ty::Record::describe()` renders field names verbatim with no escaping —
/// so a crafted field name (as could arrive from a wasm block's decoded
/// `cf_signature` JSON) can itself contain a `\0` byte and fake a node
/// boundary.
///
/// Sequence A is one node whose output record has a single field named
/// `fieldname`, crafted so its rendered signature text equals
/// `sig1 + "\0" + "b" + "\0" + sig2`. Sequence B is the two nodes `a`/`sig1`
/// and `b`/`sig2` that string is built from. Under the old delimiter-join
/// scheme these hash identically despite being different graphs; the fixed,
/// length-prefixed `graph_fingerprint` must tell them apart.
#[test]
fn graph_fingerprint_closes_the_old_delimiter_collision() {
    use cuttlefish_abi::{Signature, Ty};

    fn record_sig(field: &str) -> Signature {
        let mut fields = BTreeMap::new();
        fields.insert(field.to_string(), Ty::Text);
        Signature {
            input: Ty::Text,
            output: Ty::Record(fields),
        }
    }

    let sig1 = record_sig("g");
    let sig2 = record_sig("h");
    assert_eq!(sig1.to_string(), "text -> {g: text}");
    assert_eq!(sig2.to_string(), "text -> {h: text}");

    // Crafted so that wrapping it as `{fieldname: text}` (a record's own
    // "{" ... ": text}" framing) reproduces `sig1 + "\0" + "b" + "\0" +
    // sig2` byte-for-byte. Derivation: strip the record wrapper from sig1
    // and sig2 and splice the two around the node-2 name "b".
    let fieldname = "g: text}\0b\0text -> {h".to_string();
    let sig_a = record_sig(&fieldname);

    // Sanity-check the hand-derived algebra before trusting the rest of the
    // test: sig_a's rendered text really does equal the naive two-node join.
    let naive_two_node_join = format!("{sig1}\0b\0{sig2}");
    assert_eq!(
        sig_a.to_string(),
        naive_two_node_join,
        "the crafted field name must reproduce the two-node byte stream exactly"
    );

    let sequence_a = vec![checked_node("a", sig_a)];
    let sequence_b = vec![checked_node("a", sig1), checked_node("b", sig2)];

    // Reproduce the *old*, pre-fix `\0`-join scheme here (not called from
    // production code) purely to prove these two sequences really would
    // have collided under it.
    fn old_broken_fingerprint(nodes: &[CheckedNode]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for node in nodes {
            hasher.update(node.name.as_bytes());
            hasher.update(b"\0");
            hasher.update(node.signature.to_string().as_bytes());
            hasher.update(b"\0");
        }
        format!("{:x}", hasher.finalize())
    }
    assert_eq!(
        old_broken_fingerprint(&sequence_a),
        old_broken_fingerprint(&sequence_b),
        "sanity check: these two sequences must collide under the old delimiter-join scheme"
    );

    // The actual fix under test: the real, length-prefixed
    // `graph_fingerprint` must NOT collide on these two structurally
    // different sequences.
    assert_ne!(
        graph_fingerprint(&sequence_a),
        graph_fingerprint(&sequence_b),
        "length-prefixed hashing must distinguish sequences that collided under the old \\0-join"
    );
}

/// A more direct, types-free proof of `hash_length_prefixed`'s injectivity
/// property: feeding it `["ab", "c"]` and `["a", "bc"]` must not produce the
/// same digest, even though a plain `\0`-delimiter join of those two pairs
/// with content-embedded delimiter bytes could in principle collide. This
/// exercises the primitive itself, independent of `CheckedNode`/`Signature`
/// plumbing.
#[test]
fn hash_length_prefixed_is_injective_across_a_boundary_shift() {
    use sha2::{Digest, Sha256};

    fn hash_pair(a: &[u8], b: &[u8]) -> String {
        let mut hasher = Sha256::new();
        // Mirrors dag.rs's private `hash_length_prefixed` exactly, since
        // that function isn't exported — this is the same primitive,
        // exercised directly on raw bytes rather than through
        // `CheckedNode`.
        for part in [a, b] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        format!("{:x}", hasher.finalize())
    }

    // Under a naive `\0`-join, "a\0b" + "\0" + "c\0" and "a\0" + "b\0c" +
    // "\0" style boundary shifts are exactly the injectivity failure mode.
    // Length-prefixing must keep these distinct.
    assert_ne!(
        hash_pair(b"ab", b"c"),
        hash_pair(b"a", b"bc"),
        "shifting content across a segment boundary must change the hash"
    );

    // A segment that itself contains a literal NUL byte (standing in for a
    // block-declared signature field name with an embedded NUL) must not be
    // confusable with a delimiter-joined split at that NUL.
    assert_ne!(
        hash_pair(b"a\0b", b"c"),
        hash_pair(b"a", b"b\0c"),
        "an embedded NUL inside a segment must not be interpretable as a segment boundary"
    );
}
