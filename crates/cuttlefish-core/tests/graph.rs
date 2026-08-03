use cuttlefish_core::graph::{GraphParser, InputExpr};
use cuttlefish_core::lex::lex;

fn parse_graph(src: &str) -> cuttlefish_core::graph::NodeGraph {
    let tokens = lex(src).unwrap();
    let (graph, consumed) = GraphParser {
        tokens: &tokens,
        at: 0,
    }
    .node_graph()
    .unwrap();
    assert_eq!(
        consumed,
        tokens.len(),
        "should consume every token in these single-value tests"
    );
    graph
}

#[test]
fn a_single_node_with_no_input_parses() {
    let g = parse_graph(r#"{ files = { block = "list-dir@1" }; }"#);
    assert_eq!(g.nodes.len(), 1);
    assert_eq!(
        g.get("files").unwrap().block,
        std::path::PathBuf::from("list-dir@1")
    );
    assert!(g.get("files").unwrap().input.is_none());
}

#[test]
fn from_node_input_parses() {
    let g = parse_graph(
        r#"{ files = { block = "list-dir@1" }; chunk = { block = "chunker@1"; in = files.out; }; }"#,
    );
    assert_eq!(
        g.get("chunk").unwrap().input,
        Some(InputExpr::FromNode("files".into()))
    );
}

#[test]
fn record_fan_in_parses() {
    let g = parse_graph(
        r#"{
          docs = { block = "load-docs@1" };
          images = { block = "load-images@1" };
          report = { block = "report-writer@1"; in = { docs = docs.out; images = images.out }; };
        }"#,
    );
    match g.get("report").unwrap().input.as_ref().unwrap() {
        InputExpr::Record(fields) => {
            assert_eq!(
                fields.get("docs"),
                Some(&InputExpr::FromNode("docs".into()))
            );
            assert_eq!(
                fields.get("images"),
                Some(&InputExpr::FromNode("images".into()))
            );
        }
        other => panic!("expected Record, got {other:?}"),
    }
}

#[test]
fn list_fan_in_parses_in_order() {
    let g = parse_graph(
        r#"{
          a = { block = "a@1" };
          b = { block = "b@1" };
          combined = { block = "merge@1"; in = [a.out, b.out]; };
        }"#,
    );
    assert_eq!(
        g.get("combined").unwrap().input,
        Some(InputExpr::List(vec![
            InputExpr::FromNode("a".into()),
            InputExpr::FromNode("b".into()),
        ]))
    );
}

#[test]
fn repeat_until_without_max_iterations_is_rejected() {
    let tokens =
        lex(r#"{ refine = { block = "refiner@1"; in = draft.out; repeat_until = "done"; }; }"#)
            .unwrap();
    let result = GraphParser {
        tokens: &tokens,
        at: 0,
    }
    .node_graph();
    assert!(result.is_err());
}

#[test]
fn repeat_until_with_max_iterations_parses() {
    let g = parse_graph(
        r#"{ refine = { block = "refiner@1"; in = draft.out; repeat_until = "done"; max_iterations = 5; }; }"#,
    );
    let node = g.get("refine").unwrap();
    assert_eq!(node.repeat_until.as_deref(), Some("done"));
    assert_eq!(node.max_iterations, Some(5));
}

#[test]
fn branches_parse() {
    use cuttlefish_core::graph::Branches;
    let src = r#"{ classify = { "pdf" -> handle_pdf; "scan" -> handle_scan; }; }"#;
    let tokens = lex(src).unwrap();
    let (branches, consumed) = GraphParser {
        tokens: &tokens,
        at: 0,
    }
    .branches()
    .unwrap();
    assert_eq!(consumed, tokens.len());
    assert_eq!(
        branches.decisions,
        vec![(
            "classify".to_string(),
            vec![
                ("pdf".to_string(), "handle_pdf".to_string()),
                ("scan".to_string(), "handle_scan".to_string()),
            ]
        )]
    );
    let _ = Branches::default(); // keep type imported/used if the above changes
}

#[test]
fn a_bare_ident_not_ending_in_dot_out_is_rejected_as_an_input() {
    let tokens = lex(r#"{ chunk = { block = "chunker@1"; in = files; }; }"#).unwrap();
    assert!(GraphParser {
        tokens: &tokens,
        at: 0
    }
    .node_graph()
    .is_err());
}
