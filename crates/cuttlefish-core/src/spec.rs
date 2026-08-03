//! Parsing `Cuttlefish.spec` files.
//!
//! # Scope, and why this is a scanner rather than a parser library
//!
//! The language this project is heading toward is a typed DSL with `let`-bound
//! pipelines, block signatures, and inference over them. This is not that. It
//! reads a deliberately flat subset — a `spec NAME = { key = value; ... }` block
//! with a fixed set of keys — because that is all the first working end-to-end
//! job needs.
//!
//! Reaching for a parser-combinator library before the grammar has expressions
//! in it would be building the abstraction for a language that does not exist
//! yet, against guesses about its shape. When the pipeline syntax lands, this
//! module gets replaced rather than extended.
//!
//! # Why it refuses so much
//!
//! A spec grants capabilities. Every accepted-but-misunderstood construct is a
//! job running under permissions nobody wrote down, so anything not fully
//! understood is an error:
//!
//! - An unknown key is rejected rather than skipped. Silently ignoring one is
//!   how a misspelled `capabilities` becomes a spec with no capabilities that
//!   still runs — and looks fine.
//! - An unsupported model kind or capability kind is rejected by name, rather
//!   than being treated as the nearest supported thing.
//!
//! Being liberal in what it accepts would be exactly the wrong instinct here.

use std::path::PathBuf;
use thiserror::Error;

/// Where a job's model comes from.
///
/// Deliberately *not* an enum of known providers. Inference can come from a
/// local Ollama, an OpenAI-compatible HTTP endpoint, an embedded llama.cpp, or
/// something not thought of yet, and this crate has no business knowing which
/// of those exist — it parses job descriptions.
///
/// So a model reference is a provider name and a target, and resolving one into
/// something that can actually generate is the host's job, via its backend
/// registry. Adding a provider therefore touches neither this type nor the
/// parser: an unknown provider is a resolution error naming what *is*
/// available, not a syntax error.
///
/// In a spec this is written `model = Provider "target"`:
///
/// ```text
/// model = Ollama "llama3.2:1b";          // a local Ollama
/// model = OpenAi "http://host/v1#gpt-4"; // an OpenAI-compatible endpoint
/// model = Path "./models/qwen.gguf";     // a local file, for embedded runtimes
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// Which backend should serve this, lowercased — `ollama`, `path`, `stub`.
    ///
    /// Lowercased at parse time so that `Ollama` and `OLLAMA` name the same
    /// provider; a spec should not fail over capitalisation.
    pub provider: String,
    /// What to ask that backend for. Its meaning belongs entirely to the
    /// provider: a model tag for Ollama, a filesystem path for an embedded
    /// runtime, a URL for an HTTP endpoint.
    pub target: String,
}

impl ModelRef {
    /// Construct a reference directly, mostly for tests and for callers
    /// building a spec without parsing one.
    pub fn new(provider: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            provider: provider.into().to_lowercase(),
            target: target.into(),
        }
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider, self.target)
    }
}

/// How a job's data may be handled.
///
/// This is discovery metadata, consumed by the agent harness — it is *not*
/// enforcement. What actually gates file access is the capability list, checked
/// by the host at runtime. The distinction matters: `data_policy` tells the
/// calling *agent* to behave differently (pass paths, not contents), while
/// capabilities tell the *sandbox* what it may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPolicy {
    /// Content must not leave the machine; the agent should pass paths.
    LocalOnly,
    /// No special handling requested.
    Any,
}

/// A parsed spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// Job name, used to submit against it.
    pub name: String,
    /// Trigger conditions for a calling agent — when to use this, never how it
    /// works. A description that summarises the workflow invites an agent to
    /// act on the summary instead of reading the real contract.
    pub description: String,
    /// Which model serves this job's inference.
    pub model: ModelRef,
    /// Data-handling policy; see [`DataPolicy`].
    pub data_policy: DataPolicy,
    /// Directories this job may read beneath. Empty means none.
    pub read_roots: Vec<PathBuf>,
    /// The proc-blocks implementing the job, as a graph of nodes.
    ///
    /// Each node's declared input is typechecked against the nodes feeding
    /// it before anything runs. `block = "...";` is sugar for a one-node
    /// graph — see [`crate::graph::NodeGraph::single`].
    pub nodes: crate::graph::NodeGraph,
    /// Conditional dispatch: which branch target fires for each labeled
    /// route a branching node produces. Empty when the spec has none.
    pub branches: crate::graph::Branches,
}

/// Why a spec was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    /// A required key was absent.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A key that this version does not understand.
    #[error("unknown field `{0}`")]
    UnknownField(String),
    /// Structurally malformed input.
    #[error("malformed spec: {0}")]
    Malformed(String),
    /// A capability kind that exists in the design but not in this build.
    #[error("unsupported capability `{0}` (this build supports only `Read`)")]
    UnsupportedCapability(String),
}

use crate::lex::{lex, Tok, Token};

/// Parse a spec.
///
/// Recursive descent over tokens, not splitting on punctuation — see
/// [`crate::lex`] for why that distinction is load-bearing rather than
/// stylistic.
pub fn parse_spec(src: &str) -> Result<Spec, SpecError> {
    let tokens = lex(src).map_err(|e| SpecError::Malformed(e.to_string()))?;
    Parser {
        tokens: &tokens,
        at: 0,
    }
    .spec()
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }

    /// Describe where the parser is, for an error message.
    fn here(&self) -> String {
        match self.tokens.get(self.at) {
            Some(t) => format!("{} at {}", t.tok.describe(), t.span),
            None => "end of input".into(),
        }
    }

    fn advance(&mut self) -> Option<&'a Token> {
        let t = self.tokens.get(self.at);
        if t.is_some() {
            self.at += 1;
        }
        t
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
        match self.advance().map(|t| &t.tok) {
            Some(Tok::Ident(name)) => Ok(name.clone()),
            _ => {
                self.at = self.at.saturating_sub(1);
                Err(SpecError::Malformed(format!(
                    "expected a name, found {}",
                    self.here()
                )))
            }
        }
    }

    fn string(&mut self, field: &str) -> Result<String, SpecError> {
        match self.advance().map(|t| &t.tok) {
            Some(Tok::Str(value)) => Ok(value.clone()),
            _ => {
                self.at = self.at.saturating_sub(1);
                Err(SpecError::Malformed(format!(
                    "field `{field}` must be a quoted string, found {}",
                    self.here()
                )))
            }
        }
    }

    /// `spec NAME = { field* }`
    fn spec(&mut self) -> Result<Spec, SpecError> {
        match self.ident()?.as_str() {
            "spec" => {}
            other => {
                return Err(SpecError::Malformed(format!(
                    "a spec file starts with `spec`, found `{other}`"
                )))
            }
        }
        let name = self.ident()?;
        self.expect(&Tok::Equals)?;
        self.expect(&Tok::OpenBrace)?;

        let (mut description, mut model, mut data_policy, mut read_roots, mut nodes, mut branches) =
            (None, None, None, None, None, None);

        while self.peek().is_some() && self.peek() != Some(&Tok::CloseBrace) {
            let key = self.ident()?;
            self.expect(&Tok::Equals)?;

            match key.as_str() {
                "description" => description = Some(self.string("description")?),
                "block" => {
                    nodes = Some(crate::graph::NodeGraph::single(PathBuf::from(
                        self.string("block")?,
                    )))
                }
                "nodes" => {
                    let (g, new_at) = crate::graph::GraphParser {
                        tokens: self.tokens,
                        at: self.at,
                    }
                    .node_graph()?;
                    self.at = new_at; // advance Parser's own cursor past what GraphParser consumed
                    nodes = Some(g);
                }
                "branches" => {
                    let (b, new_at) = crate::graph::GraphParser {
                        tokens: self.tokens,
                        at: self.at,
                    }
                    .branches()?;
                    self.at = new_at;
                    branches = Some(b);
                }
                "capabilities" => read_roots = Some(self.capabilities()?),
                "model" => model = Some(self.model()?),
                "data_policy" => {
                    data_policy = Some(match self.ident()?.as_str() {
                        "Local_only" => DataPolicy::LocalOnly,
                        "Any" => DataPolicy::Any,
                        other => {
                            return Err(SpecError::Malformed(format!(
                                "unknown data_policy `{other}`"
                            )))
                        }
                    })
                }
                other => return Err(SpecError::UnknownField(other.to_string())),
            }

            // A trailing semicolon is conventional but not required — and,
            // unlike before, one *inside* a string is just a character.
            if self.peek() == Some(&Tok::Semicolon) {
                self.at += 1;
            }
        }
        self.expect(&Tok::CloseBrace)?;

        Ok(Spec {
            name,
            description: description.ok_or(SpecError::MissingField("description"))?,
            model: model.ok_or(SpecError::MissingField("model"))?,
            data_policy: data_policy.ok_or(SpecError::MissingField("data_policy"))?,
            read_roots: read_roots.ok_or(SpecError::MissingField("capabilities"))?,
            nodes: nodes.ok_or(SpecError::MissingField("block"))?,
            branches: branches.unwrap_or_default(),
        })
    }

    /// `Provider "target"`
    fn model(&mut self) -> Result<ModelRef, SpecError> {
        let provider = self.ident()?;
        if provider.is_empty() || !provider.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(SpecError::Malformed(format!(
                "`{provider}` is not a valid model provider name"
            )));
        }
        Ok(ModelRef::new(provider, self.string("model")?))
    }

    /// `[ Read "a", Read "b" ]`
    fn capabilities(&mut self) -> Result<Vec<PathBuf>, SpecError> {
        let mut roots = Vec::new();
        self.expect(&Tok::OpenBracket)?;
        while self.peek() != Some(&Tok::CloseBracket) {
            let kind = self.ident()?;
            if kind != "Read" {
                return Err(SpecError::UnsupportedCapability(kind));
            }
            roots.push(PathBuf::from(self.string("capabilities")?));
            if self.peek() == Some(&Tok::Comma) {
                self.at += 1;
            } else {
                break;
            }
        }
        self.expect(&Tok::CloseBracket)?;
        Ok(roots)
    }
}
