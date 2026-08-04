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
use cuttlefish_host::dag::{check_graph, BranchExclusivity, DagError};
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
