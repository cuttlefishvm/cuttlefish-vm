//! The graph AST: `nodes = {...}` and `branches = {...}`.
//!
//! A node's `in` is an expression over other nodes' outputs, not just a bare
//! name — `Record`/`List` are what make fan-in possible. See
//! `docs/superpowers/specs/2026-08-03-dag-core-design.md` for the full
//! rationale; this module is purely the parsed shape, with no typechecking
//! or execution logic (those live in `cuttlefish-host`).

use crate::lex::{Tok, Token};
use crate::spec::SpecError;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What feeds a node's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputExpr {
    /// `other_node.out` — this node's whole output.
    FromNode(String),
    /// `{ field = expr; ... }` — build a record from several nodes.
    Record(BTreeMap<String, InputExpr>),
    /// `[ expr, expr, ... ]` — build a list from several nodes, order significant.
    List(Vec<InputExpr>),
}

/// One node in the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The block/bundle this node runs — same string shape as today's
    /// `pipeline` entries (a path, or a bare `name@version`).
    pub block: PathBuf,
    /// What feeds this node. `None` only for a node with no inbound edge —
    /// the graph's entry point(s).
    pub input: Option<InputExpr>,
    /// Bounded-loop marker: the `Ty::Text` output field compared against
    /// `"done"`. Requires `max_iterations`.
    pub repeat_until: Option<String>,
    /// Mandatory alongside `repeat_until` — enforced at parse time, not left
    /// as a typecheck-time gap, since a missing bound is a spec-authoring
    /// mistake regardless of what the graph shape turns out to be.
    pub max_iterations: Option<u32>,
}

/// `nodes = { name = { ... }; ... }`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeGraph {
    /// Insertion order preserved (`BTreeMap` would reorder alphabetically,
    /// which is fine for lookup but wrong for any diagnostic that lists
    /// nodes "in the order the author wrote them").
    pub nodes: Vec<(String, Node)>,
}

impl NodeGraph {
    /// The one-node graph `block = "...";` desugars to.
    pub fn single(block: PathBuf) -> Self {
        Self {
            nodes: vec![(
                "block".to_string(),
                Node {
                    block,
                    input: None,
                    repeat_until: None,
                    max_iterations: None,
                },
            )],
        }
    }

    /// Look up a node by name.
    pub fn get(&self, name: &str) -> Option<&Node> {
        self.nodes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, node)| node)
    }
}

/// `branches = { node_name = { "label" -> target; ... }; ... }`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Branches {
    /// (branching node name) -> (label -> target node name), insertion order.
    pub decisions: Vec<(String, Vec<(String, String)>)>,
}

/// A self-contained recursive-descent parser for the `nodes = {...}` and
/// `branches = {...}` bodies, operating directly on a token slice.
pub struct GraphParser<'a> {
    /// The full token stream being parsed.
    pub tokens: &'a [Token],
    /// Current cursor position into `tokens`.
    pub at: usize,
}

impl<'a> GraphParser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }
    fn here(&self) -> String {
        match self.tokens.get(self.at) {
            Some(t) => format!("{} at {}", t.tok.describe(), t.span),
            None => "end of input".into(),
        }
    }
    fn expect(&mut self, want: &Tok) -> Result<(), SpecError> {
        match self.peek() {
            Some(got) if got == want => {
                self.at += 1;
                Ok(())
            }
            _ => Err(SpecError::Malformed(format!(
                "expected {}, found {}",
                want.describe(),
                self.here()
            ))),
        }
    }
    fn ident(&mut self) -> Result<String, SpecError> {
        match self.tokens.get(self.at).map(|t| &t.tok) {
            Some(Tok::Ident(name)) => {
                self.at += 1;
                Ok(name.clone())
            }
            _ => Err(SpecError::Malformed(format!(
                "expected a name, found {}",
                self.here()
            ))),
        }
    }
    fn string(&mut self) -> Result<String, SpecError> {
        match self.tokens.get(self.at).map(|t| &t.tok) {
            Some(Tok::Str(s)) => {
                self.at += 1;
                Ok(s.clone())
            }
            _ => Err(SpecError::Malformed(format!(
                "expected a quoted string, found {}",
                self.here()
            ))),
        }
    }
    fn skip_semi(&mut self) {
        if self.peek() == Some(&Tok::Semicolon) {
            self.at += 1;
        }
    }

    /// `{ name = { field* }; ... }` — the whole `nodes = {...}` body.
    ///
    /// Returns the parsed graph together with the token position just past
    /// its closing `}` — `spec.rs`'s `Parser` (a *separate* struct walking
    /// the same token slice, Task 3) needs that position to resume parsing
    /// the rest of the `spec {...}` body afterward, since it can't see
    /// `GraphParser`'s internal cursor otherwise.
    pub fn node_graph(&mut self) -> Result<(NodeGraph, usize), SpecError> {
        self.expect(&Tok::OpenBrace)?;
        let mut nodes = Vec::new();
        while self.peek().is_some() && self.peek() != Some(&Tok::CloseBrace) {
            let name = self.ident()?;
            self.expect(&Tok::Equals)?;
            let node = self.node_body()?;
            nodes.push((name, node));
            self.skip_semi();
        }
        self.expect(&Tok::CloseBrace)?;
        if nodes.is_empty() {
            return Err(SpecError::Malformed("nodes needs at least one node".into()));
        }
        Ok((NodeGraph { nodes }, self.at))
    }

    /// `{ block = "..."; in = expr; repeat_until = "..."; max_iterations = N; }`
    fn node_body(&mut self) -> Result<Node, SpecError> {
        self.expect(&Tok::OpenBrace)?;
        let (mut block, mut input, mut repeat_until, mut max_iterations) = (None, None, None, None);
        while self.peek().is_some() && self.peek() != Some(&Tok::CloseBrace) {
            let key = self.ident()?;
            self.expect(&Tok::Equals)?;
            match key.as_str() {
                "block" => block = Some(PathBuf::from(self.string()?)),
                "in" => input = Some(self.input_expr()?),
                "repeat_until" => repeat_until = Some(self.string_or_field()?),
                "max_iterations" => max_iterations = Some(self.number()?),
                other => return Err(SpecError::UnknownField(other.to_string())),
            }
            self.skip_semi();
        }
        self.expect(&Tok::CloseBrace)?;
        if let (Some(_), None) = (&repeat_until, &max_iterations) {
            return Err(SpecError::Malformed(
                "repeat_until requires max_iterations".into(),
            ));
        }
        Ok(Node {
            block: block.ok_or(SpecError::MissingField("block"))?,
            input,
            repeat_until,
            max_iterations,
        })
    }

    /// `repeat_until = "done"` — a bare field-name string, not a node
    /// reference, so this reuses `string()` (kept as its own method name at
    /// the call site above for readability, not because parsing differs).
    fn string_or_field(&mut self) -> Result<String, SpecError> {
        self.string()
    }

    fn number(&mut self) -> Result<u32, SpecError> {
        // Numbers aren't tokenized separately today (see lex.rs) — an
        // integer like `5` lexes as `Ident("5")` since digits satisfy
        // `is_alphanumeric()`. Parsing it here, rather than adding a
        // dedicated numeric token, keeps this the only place that cares.
        let s = self.ident()?;
        s.parse::<u32>()
            .map_err(|_| SpecError::Malformed(format!("`{s}` is not a valid max_iterations")))
    }

    /// `node.out` | `{ field = expr; ... }` | `[ expr, ... ]`
    fn input_expr(&mut self) -> Result<InputExpr, SpecError> {
        match self.peek() {
            Some(Tok::OpenBrace) => {
                self.at += 1;
                let mut fields = BTreeMap::new();
                while self.peek() != Some(&Tok::CloseBrace) {
                    let field = self.ident()?;
                    self.expect(&Tok::Equals)?;
                    fields.insert(field, self.input_expr()?);
                    self.skip_semi();
                }
                self.expect(&Tok::CloseBrace)?;
                Ok(InputExpr::Record(fields))
            }
            Some(Tok::OpenBracket) => {
                self.at += 1;
                let mut items = Vec::new();
                while self.peek() != Some(&Tok::CloseBracket) {
                    items.push(self.input_expr()?);
                    if self.peek() == Some(&Tok::Comma) {
                        self.at += 1;
                    } else {
                        break;
                    }
                }
                self.expect(&Tok::CloseBracket)?;
                Ok(InputExpr::List(items))
            }
            Some(Tok::Ident(reference)) => {
                let reference = reference.clone();
                self.at += 1;
                reference
                    .strip_suffix(".out")
                    .map(|node| InputExpr::FromNode(node.to_string()))
                    .ok_or_else(|| {
                        SpecError::Malformed(format!(
                            "`{reference}` is not a node reference — expected `<node>.out`"
                        ))
                    })
            }
            _ => Err(SpecError::Malformed(format!(
                "expected a node reference, `{{...}}`, or `[...]`, found {}",
                self.here()
            ))),
        }
    }

    /// `{ node_name = { "label" -> target; ... }; ... }` — the whole
    /// `branches = {...}` body. Returns `(Branches, new_at)`, same handoff
    /// convention as [`Self::node_graph`].
    pub fn branches(&mut self) -> Result<(Branches, usize), SpecError> {
        self.expect(&Tok::OpenBrace)?;
        let mut decisions = Vec::new();
        while self.peek().is_some() && self.peek() != Some(&Tok::CloseBrace) {
            let node_name = self.ident()?;
            self.expect(&Tok::Equals)?;
            self.expect(&Tok::OpenBrace)?;
            let mut labels = Vec::new();
            while self.peek() != Some(&Tok::CloseBrace) {
                let label = self.string()?;
                self.expect(&Tok::Arrow)?;
                let target = self.ident()?;
                labels.push((label, target));
                self.skip_semi();
            }
            self.expect(&Tok::CloseBrace)?;
            decisions.push((node_name, labels));
            self.skip_semi();
        }
        self.expect(&Tok::CloseBrace)?;
        Ok((Branches { decisions }, self.at))
    }
}
