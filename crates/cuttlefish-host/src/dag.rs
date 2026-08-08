//! Typechecking a node graph: topological order, fan-in composition via
//! `InputExpr`, cycle rejection (unless marked `repeat_until`), and
//! branch-exclusivity analysis for conditional dispatch.
//!
//! See docs/superpowers/specs/2026-08-03-dag-core-design.md for the full
//! design. This module's `check_graph` is the graph-shaped analogue of
//! `crate::pipeline::check` — same idea (typecheck seams before anything
//! runs), different shape (a graph of named nodes instead of a `Vec`).

use crate::pipeline::{read_stage_signature, PipelineError, ResolvedInput};
use cuttlefish_abi::Ty;
use cuttlefish_core::graph::{Branches, InputExpr, NodeGraph};
use std::collections::{BTreeMap, HashMap, HashSet};
use wasmtime::Engine;

/// One checked node, ready to execute.
///
/// `Clone` because a later task's `resume_job` rebuilds a `JobSpec` from an
/// `Arc<Vec<CheckedNode>>` held in `AppState` — every field type here
/// (`Signature`, `ArtifactKind`, `InputExpr`) is already `Clone`.
#[derive(Clone)]
pub struct CheckedNode {
    /// The node's name, as written in `nodes = {...}`.
    pub name: String,
    /// Block or bundle.
    pub kind: crate::catalog::ArtifactKind,
    /// The exact `name@version` this node resolved to, if it came from the
    /// catalog.
    pub resolved: Option<String>,
    /// The compiled module (block) or `.cfbundle` (bundle) bytes.
    pub module_bytes: Vec<u8>,
    /// What it declared.
    pub signature: cuttlefish_abi::Signature,
    /// What feeds this node, if anything.
    pub input: Option<InputExpr>,
    /// Bounded-loop marker, if this node re-runs on its own output.
    pub repeat_until: Option<String>,
    /// Iteration bound, required alongside `repeat_until`.
    pub max_iterations: Option<u32>,
    /// Threaded straight from `ResolvedInput::script`/`Stage::script` — see
    /// `pipeline.rs`. `Some` only for a `Script`-kind node.
    pub script: Option<String>,
    /// The fan-out manifest this node runs over, if any — see
    /// [`cuttlefish_core::graph::Node::over`]. When set, this node runs its
    /// block once per manifest line and presents
    /// [`fanout_collection_ty`] downstream rather than its block's own
    /// declared output.
    pub over: Option<std::path::PathBuf>,
}

/// What a fan-out node presents to the nodes downstream of it.
///
/// Deliberately *not* the block's own declared output: downstream consumes
/// the collection of every item's result, not any one item's. The counts are
/// [`Ty::Json`] because [`Ty`] has no number variant.
pub fn fanout_collection_ty() -> Ty {
    Ty::Record(BTreeMap::from([
        ("results_path".to_string(), Ty::Text),
        ("failures_path".to_string(), Ty::Text),
        ("succeeded".to_string(), Ty::Json),
        ("failed".to_string(), Ty::Json),
    ]))
}

/// A whole graph, typechecked and topologically ordered.
pub struct CheckedGraph {
    /// In topological order — safe to execute front-to-back, threading
    /// `outputs` forward, per the spec's execution-semantics section.
    pub nodes: Vec<CheckedNode>,
    /// Which nodes are exclusive to which branch label, keyed by the
    /// branching node's name — see "skip propagation" in `check_graph`.
    pub exclusive_to: HashMap<String, BranchExclusivity>,
}

/// Which branch decision + label a node is exclusive to — a node is only
/// executed when this decision's chosen route matches `label`. Carrying
/// `decision` (not just `label`) is what lets two independent `branches`
/// decisions that happen to reuse the same label string coexist without
/// being confused for a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchExclusivity {
    /// The branching decision (key in `branches.decisions`) this exclusivity
    /// belongs to.
    pub decision: String,
    /// The label within that decision.
    pub label: String,
}

/// Why a graph was rejected.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    /// A per-node signature lookup or seam check failed the same way a
    /// linear pipeline's would.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    /// A node's `in` expression names a node that doesn't exist.
    #[error("node `{node}` references unknown node `{referenced}`")]
    UnknownReference {
        /// The node whose `in` expression has the bad reference.
        node: String,
        /// The name it referenced.
        referenced: String,
    },
    /// A node has an inbound edge that would form a cycle without an
    /// explicit `repeat_until` marker.
    #[error(
        "node `{node}` has an inbound edge that would form a cycle with no \
         repeat_until marker — add one (with max_iterations) if this loop is intentional"
    )]
    UnmarkedCycle {
        /// The node that could not be ordered.
        node: String,
    },
    /// A node's input mixes outputs from two mutually-exclusive branch labels
    /// of the same decision.
    #[error(
        "node `{node}`'s input mixes output from branch label `{label_a}` and \
         label `{label_b}` of the same `branches.{decision}` decision — a node \
         cannot depend on more than one mutually-exclusive branch outcome at once"
    )]
    ConflictingBranchFanIn {
        /// The node whose input mixes two labels.
        node: String,
        /// The branching decision both labels belong to.
        decision: String,
        /// The first label found.
        label_a: String,
        /// The second, conflicting label.
        label_b: String,
    },
    /// A node's declared input doesn't accept what its `in` expression
    /// produces.
    #[error("node `{consumer}` needs {expected}, but `{producer}` produces {produced}")]
    SeamMismatch {
        /// What produced the mismatched value.
        producer: String,
        /// What it produces.
        produced: String,
        /// The node that needed something else.
        consumer: String,
        /// What it needs.
        expected: String,
    },
    /// The graph had no nodes.
    #[error("a graph needs at least one node")]
    Empty,
}

/// Typecheck a graph. `resolved` must already contain one entry per node in
/// `graph.nodes`, in the same order — building that mapping (via
/// `pipeline::resolve_and_load`, one call per node) is the caller's job
/// (a later task), same division of responsibility `pipeline::check` already has.
pub fn check_graph(
    engine: &Engine,
    graph: &NodeGraph,
    branches: &Branches,
    resolved: &HashMap<String, ResolvedInput>,
) -> Result<CheckedGraph, DagError> {
    if graph.nodes.is_empty() {
        return Err(DagError::Empty);
    }

    // 1. Read every node's signature up front — needed before topological
    //    evaluation since InputExpr composition needs to know each
    //    referenced node's *output* type, and a node can be referenced
    //    before it's "current" in visit order.
    let mut signatures = HashMap::new();
    for (name, node) in &graph.nodes {
        let input = resolved.get(name).expect("caller resolved every node");
        let mut signature = read_stage_signature(engine, input)?;
        if node.over.is_some() {
            // A fan-out node's block declares the shape of *one item's*
            // result, but downstream nodes consume the collection of all of
            // them. Substituting here — where per-node signatures are first
            // collected — is what makes every seam check downstream correct
            // with no further changes, since `evaluate_expr_ty` and each
            // `assignable_to` comparison read from this same map. The
            // block's own declared output is still used, per item, to
            // validate what each run produced.
            signature.output = fanout_collection_ty();
        }
        signatures.insert(name.clone(), signature);
    }

    // 2. Topological sort with cycle detection. An edge node -> referenced
    //    is only legal going "backward" (referenced already visited) unless
    //    `node` declares repeat_until, in which case a self-edge is exactly
    //    what's expected and not an error.
    let order = topological_order(graph)?;

    // 3. Branch-exclusivity: for each `branches` decision, walk forward from
    //    each label's target, marking every node whose InputExpr needs that
    //    target (transitively) as exclusive to that label. A node needing
    //    two labels of the *same* decision is a build-time error.
    let exclusive_to = compute_branch_exclusivity(graph, branches)?;

    // 4. For each node in topological order, evaluate its InputExpr into a
    //    Ty (composing referenced nodes' output types) and check
    //    assignable_to against its own declared input.
    let mut nodes = Vec::with_capacity(order.len());
    for name in &order {
        let node = graph
            .get(name)
            .expect("topological_order only returns known nodes");
        let input_resolved = resolved.get(name).expect("caller resolved every node");
        let signature = signatures.get(name).unwrap().clone();

        if let Some(expr) = &node.input {
            let produced = evaluate_expr_ty(expr, &signatures, graph)?;
            if !produced.assignable_to(&signature.input) {
                let (producer, produced_str) = describe_expr(expr, &signatures);
                return Err(DagError::SeamMismatch {
                    producer,
                    produced: produced_str,
                    consumer: name.clone(),
                    expected: signature.input.to_string(),
                });
            }
        }

        nodes.push(CheckedNode {
            name: name.clone(),
            kind: input_resolved.kind,
            resolved: input_resolved.resolved.clone(),
            module_bytes: input_resolved.bytes.clone(),
            signature,
            input: node.input.clone(),
            repeat_until: node.repeat_until.clone(),
            max_iterations: node.max_iterations,
            script: input_resolved.script.clone(),
            over: node.over.clone(),
        });
    }

    Ok(CheckedGraph {
        nodes,
        exclusive_to,
    })
}

/// A stable fingerprint of a checked graph's shape and contents — every
/// node's name and declared signature, joined and hashed. Two graphs with
/// the same fingerprint are the same, for resume-safety purposes; this
/// isn't a security boundary, just a "did the loaded spec actually change"
/// guard, so a simple SHA-256 (already a workspace dependency, same crate
/// the catalog's own content-hashing uses) is all this needs.
pub fn graph_fingerprint(nodes: &[CheckedNode]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for node in nodes {
        hash_length_prefixed(&mut hasher, node.name.as_bytes());
        hash_length_prefixed(&mut hasher, node.signature.to_string().as_bytes());
        // A node fanning out over a different manifest is a different node
        // for resume purposes even with an identical signature: its recorded
        // item indices refer to inputs from the old manifest. Without this,
        // repointing `over` would resume against checkpoints computed from
        // entirely different data.
        hash_length_prefixed(
            &mut hasher,
            node.over
                .as_ref()
                .map(|p| p.as_os_str().as_encoded_bytes())
                .unwrap_or(b""),
        );
    }
    format!("{:x}", hasher.finalize())
}

/// Feed `bytes` into `hasher` prefixed with its own length (as a fixed
/// 8-byte big-endian `u64`), rather than delimiting with a fixed byte —
/// delimiter-joining is only injective if the delimiter can never appear
/// inside the content itself, which isn't guaranteed here (a wasm block's
/// declared signature can contain arbitrary field-name text, including, in
/// principle, an embedded NUL byte decoded from its `cf_signature` export's
/// JSON). Length-prefixing has no such assumption.
fn hash_length_prefixed(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Kahn's algorithm, with the repeat_until exception: an edge from `node`
/// back to itself (or forming a cycle) is only legal when `node` declares
/// `repeat_until` — everything else with an unresolved inbound edge after
/// the sort terminates is an undeclared cycle.
fn topological_order(graph: &NodeGraph) -> Result<Vec<String>, DagError> {
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (name, node) in &graph.nodes {
        deps.entry(name).or_default();
        if let Some(expr) = &node.input {
            for referenced in referenced_nodes(expr) {
                if graph.get(referenced).is_none() {
                    return Err(DagError::UnknownReference {
                        node: name.clone(),
                        referenced: referenced.to_string(),
                    });
                }
                // A node's own repeat_until self-reference is not a real
                // dependency edge for ordering purposes — it re-runs on its
                // own prior output, which the executor (a later task)
                // special-cases, not the topological sort. A self-reference
                // WITHOUT repeat_until is not this case — it's exactly the
                // undeclared-cycle mistake the spec's `repeat_until`-as-sole-
                // marker rule exists to catch, so it must still become a
                // dependency edge (on itself) that can never be satisfied,
                // tripping UnmarkedCycle below.
                let is_marked_self_loop = referenced == name && node.repeat_until.is_some();
                if !is_marked_self_loop {
                    deps.entry(name).or_default().insert(referenced);
                }
            }
        }
    }

    let mut order = Vec::new();
    let mut remaining: HashMap<&str, HashSet<&str>> = deps.clone();
    loop {
        let ready: Vec<&str> = remaining
            .iter()
            .filter(|(_, d)| d.is_empty())
            .map(|(n, _)| *n)
            .collect();
        if ready.is_empty() {
            break;
        }
        let mut ready = ready;
        ready.sort(); // deterministic order among independent nodes
        for n in &ready {
            order.push(n.to_string());
            remaining.remove(n);
        }
        for deps in remaining.values_mut() {
            for n in &ready {
                deps.remove(n);
            }
        }
    }

    if order.len() != graph.nodes.len() {
        let stuck = graph
            .nodes
            .iter()
            .map(|(n, _)| n.as_str())
            .find(|n| !order.contains(&n.to_string()))
            .unwrap();
        return Err(DagError::UnmarkedCycle {
            node: stuck.to_string(),
        });
    }
    Ok(order)
}

fn referenced_nodes(expr: &InputExpr) -> Vec<&str> {
    match expr {
        InputExpr::FromNode(n) => vec![n.as_str()],
        InputExpr::Record(fields) => fields.values().flat_map(referenced_nodes).collect(),
        InputExpr::List(items) => items.iter().flat_map(referenced_nodes).collect(),
    }
}

/// Compose an `InputExpr` into the `Ty` it produces, by looking up each
/// referenced node's declared output type.
// `graph` is only threaded through the recursive calls today (not read at
// any base case); kept as a parameter rather than dropped since a future
// case is plausible to need it (e.g. validating a reference more richly
// than `signatures` alone allows) and this is purely a lint suppression,
// not a change in behavior.
#[allow(clippy::only_used_in_recursion)]
fn evaluate_expr_ty(
    expr: &InputExpr,
    signatures: &HashMap<String, cuttlefish_abi::Signature>,
    graph: &NodeGraph,
) -> Result<Ty, DagError> {
    match expr {
        InputExpr::FromNode(n) => Ok(signatures
            .get(n)
            .ok_or_else(|| DagError::UnknownReference {
                node: "?".into(),
                referenced: n.clone(),
            })?
            .output
            .clone()),
        InputExpr::Record(fields) => {
            let mut out = BTreeMap::new();
            for (k, v) in fields {
                out.insert(k.clone(), evaluate_expr_ty(v, signatures, graph)?);
            }
            Ok(Ty::Record(out))
        }
        InputExpr::List(items) => {
            // All list items must agree on a type for `Ty::List(T)` to mean
            // anything; take the first and let assignable_to's own equality
            // fall through to a SeamMismatch if a later one disagrees. (A
            // more precise per-item error is a reasonable follow-up; this is
            // the minimal correct behavior for v1.)
            let first = items.first().ok_or_else(|| DagError::UnknownReference {
                node: "?".into(),
                referenced: "<empty list>".into(),
            })?;
            Ok(Ty::List(Box::new(evaluate_expr_ty(
                first, signatures, graph,
            )?)))
        }
    }
}

fn describe_expr(
    expr: &InputExpr,
    signatures: &HashMap<String, cuttlefish_abi::Signature>,
) -> (String, String) {
    match expr {
        InputExpr::FromNode(n) => (
            n.clone(),
            signatures
                .get(n)
                .map(|s| s.output.to_string())
                .unwrap_or_default(),
        ),
        InputExpr::Record(fields) => (
            "<composite>".to_string(),
            match same_node_repeated_across_every_field(fields) {
                // The single most common way to reach for `Record` wrong:
                // wanting one upstream node's whole output passed through
                // to a downstream node whose input shape happens to match
                // it, but writing `{ a = x.out; b = x.out; c = x.out; }`
                // instead of the bare `in = x.out;` a straight pass-through
                // actually needs. The wrapped form nests x's whole output
                // under *each* field instead of using its fields directly,
                // producing the double-nested type this message would
                // otherwise show with no explanation. Caught here rather
                // than left to be found by trial and error against a
                // confusing type dump -- see the cuttlefish-build skill.
                Some(node) => format!(
                    "{expr:?} -- every field here maps to `{node}.out`; if you meant to pass \
                     `{node}`'s whole output through unchanged, write `in = {node}.out;` with no \
                     braces instead of wrapping it field by field"
                ),
                None => format!("{expr:?}"),
            },
        ),
        other => ("<composite>".to_string(), format!("{other:?}")),
    }
}

/// If every field of a `Record` maps to the same single node's whole
/// output (`FromNode`), returns that node's name — see the call site in
/// [`describe_expr`] for why this is worth detecting.
fn same_node_repeated_across_every_field(fields: &BTreeMap<String, InputExpr>) -> Option<&str> {
    let mut names = fields.values().map(|v| match v {
        InputExpr::FromNode(n) => Some(n.as_str()),
        _ => None,
    });
    let first = names.next()??;
    names.all(|n| n == Some(first)).then_some(first)
}

/// Implements the spec's "Conditional dispatch and skipped nodes" rule
/// exactly: a node is exclusive to label L if any node its InputExpr
/// references is exclusive to L (transitively, from L's branch target). A
/// node that would be exclusive to two labels of the *same* decision at
/// once is a build-time error.
fn compute_branch_exclusivity(
    graph: &NodeGraph,
    branches: &Branches,
) -> Result<HashMap<String, BranchExclusivity>, DagError> {
    let mut exclusive_to: HashMap<String, BranchExclusivity> = HashMap::new();

    // Validate every branch target actually names a real node before
    // seeding anything — an undeclared target is a build-time error (the
    // node it should have gated instead runs unconditionally, which is
    // exactly the silent-wrong-behavior this typechecker exists to prevent),
    // named against the decision (branching node) that references it.
    for (decision, labels) in &branches.decisions {
        for (label, target) in labels {
            if graph.get(target).is_none() {
                return Err(DagError::UnknownReference {
                    node: decision.clone(),
                    referenced: target.clone(),
                });
            }
            exclusive_to.insert(
                target.clone(),
                BranchExclusivity {
                    decision: decision.clone(),
                    label: label.clone(),
                },
            );
        }
    }

    // Propagate: repeat until no new node gains an exclusivity marker.
    // O(n^2) worst case, fine at this scale (specs have tens of nodes, not
    // thousands) — larger-scale fan-out is explicitly deferred to a future
    // cycle, not this one.
    loop {
        let mut changed = false;
        for (name, node) in &graph.nodes {
            let Some(expr) = &node.input else { continue };
            let mut found: Option<BranchExclusivity> = None;
            for referenced in referenced_nodes(expr) {
                if let Some(ex) = exclusive_to.get(referenced) {
                    match &found {
                        None => found = Some(ex.clone()),
                        Some(existing)
                            if existing.decision == ex.decision && existing.label != ex.label =>
                        {
                            return Err(DagError::ConflictingBranchFanIn {
                                node: name.clone(),
                                decision: ex.decision.clone(),
                                label_a: existing.label.clone(),
                                label_b: ex.label.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            if let Some(ex) = found {
                if exclusive_to.insert(name.clone(), ex).is_none() {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    Ok(exclusive_to)
}
