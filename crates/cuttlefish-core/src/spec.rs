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
//! The `nodes = { ... }` / `branches = { ... }` graph syntax added since is not
//! that pipeline syntax: it is still the same flat `key = value` grammar, just
//! shaped to describe a graph, with no expressions or inference beyond the
//! `node.out` reference syntax itself.
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

use std::path::{Path, PathBuf};
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// URL prefixes this job may fetch, from `Fetch "https://host/path"`.
    ///
    /// An allowlist by prefix, exactly as `read_roots` is for the
    /// filesystem: a corpus that lives on the web is still a corpus, and the
    /// capability list has to describe reaching it or it stops being a
    /// truthful account of what the job touches. Empty means the job cannot
    /// fetch anything, which is the default.
    pub fetch_prefixes: Vec<String>,
    /// The model serving `embed`, if the spec declares one.
    ///
    /// Separate from `model` on purpose: an embedding model and a chat model
    /// are different things, and reusing one field would let a spec ask a
    /// chat model for vectors — which either fails or returns something
    /// shaped like an embedding that is not one.
    pub embedding_model: Option<ModelRef>,
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
    #[error("unsupported capability `{0}` (supported: `Read`, `Fetch`)")]
    UnsupportedCapability(String),
}

use crate::lex::{lex, Tok, Token};

/// Whether `root` contains `candidate`, comparing on meaningful path
/// components only.
///
/// Not `Path::starts_with`: a leading `./` is a real `Component::CurDir`, so
/// `Path::new("./corpus/m.jsonl").starts_with("corpus")` is `false` — and a
/// spec author writing `capabilities = [ Read "corpus" ]` beside
/// `over = "./corpus/m.jsonl"` has no reason to expect one spelling of the
/// same directory to be rejected.
fn path_covers(root: &Path, candidate: &Path) -> bool {
    fn significant(p: &Path) -> Vec<std::ffi::OsString> {
        p.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .map(|c| c.as_os_str().to_os_string())
            .collect()
    }
    candidate_starts_with(&significant(candidate), &significant(root))
}

fn candidate_starts_with(candidate: &[std::ffi::OsString], root: &[std::ffi::OsString]) -> bool {
    candidate.len() >= root.len() && candidate[..root.len()] == *root
}

impl Spec {
    /// Refuse a spec whose host-read paths sit outside its granted roots.
    ///
    /// Fan-out manifests and acceptance schemas are read by the *host*,
    /// which is not sandboxed — so nothing would otherwise stop either
    /// reading a path the spec never granted. Requiring them inside a
    /// declared `Read` root keeps the capability list a truthful description
    /// of everything the job touches, which is the property the whole
    /// capability model rests on.
    ///
    /// Call this **after** resolving `read_roots`, `over` and schema paths
    /// against the spec's directory, so both sides are absolute and
    /// canonical. Comparing them as written cannot work: a spec that grants
    /// an absolute root and names a relative manifest is entirely ordinary,
    /// and lexically a relative path never starts with an absolute one.
    pub fn validate_host_read_paths(&self) -> Result<(), SpecError> {
        for (name, node) in &self.nodes.nodes {
            if let Some(manifest) = &node.over {
                if !self
                    .read_roots
                    .iter()
                    .any(|root| path_covers(root, manifest))
                {
                    return Err(SpecError::Malformed(format!(
                        "node `{name}`'s manifest {} is outside every path granted by \
                         `capabilities` — add a `Read` covering it. Granted: {}",
                        manifest.display(),
                        roots_for_error(&self.read_roots)
                    )));
                }
            }
            for check in &node.accept {
                if let crate::graph::AcceptCheck::Schema(schema) = check {
                    if !self.read_roots.iter().any(|root| path_covers(root, schema)) {
                        return Err(SpecError::Malformed(format!(
                            "node `{name}`'s accept schema {} is outside every path granted \
                             by `capabilities` — add a `Read` covering it. Granted: {}",
                            schema.display(),
                            roots_for_error(&self.read_roots)
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Granted roots, for an error message.
///
/// Listed because the failure is nearly always a path that looks right: the
/// grant and the target differ by a symlink, or by one being relative. Naming
/// what *was* granted turns a guess into a comparison.
fn roots_for_error(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "(nothing)".to_string();
    }
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

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

        let mut fetch_prefixes: Vec<String> = Vec::new();
        let mut embedding_model: Option<ModelRef> = None;
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
                "embedding_model" => embedding_model = Some(self.model()?),
                "capabilities" => {
                    let (roots, fetch) = self.capabilities()?;
                    read_roots = Some(roots);
                    fetch_prefixes = fetch;
                }
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

        let read_roots = read_roots.ok_or(SpecError::MissingField("capabilities"))?;
        let nodes = nodes.ok_or(SpecError::MissingField("block"))?;

        // The equivalent check on manifests and acceptance schemas lives in
        // `validate_host_read_paths`, called once these paths have been
        // resolved against the spec's directory. It cannot be done here: a
        // spec may grant an absolute root and name a relative manifest, and
        // comparing those as written is not merely imprecise but *always*
        // wrong — a relative path can never begin with an absolute one, so
        // every such spec was rejected regardless of where the file actually
        // sat.

        Ok(Spec {
            name,
            description: description.ok_or(SpecError::MissingField("description"))?,
            model: model.ok_or(SpecError::MissingField("model"))?,
            data_policy: data_policy.ok_or(SpecError::MissingField("data_policy"))?,
            read_roots,
            fetch_prefixes,
            embedding_model,
            nodes,
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

    /// `[ Read "a", Fetch "https://example.org/" ]`
    ///
    /// Returns the read roots and the fetch prefixes separately: they grant
    /// different things and are checked by different code, and collapsing
    /// them into one list would make "what may this job reach" require
    /// reading the strings to find out.
    fn capabilities(&mut self) -> Result<(Vec<PathBuf>, Vec<String>), SpecError> {
        let (mut roots, mut fetch) = (Vec::new(), Vec::new());
        self.expect(&Tok::OpenBracket)?;
        while self.peek() != Some(&Tok::CloseBracket) {
            let kind = self.ident()?;
            match kind.as_str() {
                "Read" => roots.push(PathBuf::from(self.string("capabilities")?)),
                "Fetch" => {
                    let prefix = self.string("capabilities")?;
                    // A prefix that is not a URL cannot match anything, so it
                    // is an authoring mistake worth catching now rather than
                    // as a puzzling denial at runtime.
                    if !prefix.starts_with("http://") && !prefix.starts_with("https://") {
                        return Err(SpecError::Malformed(format!(
                            "`Fetch {prefix:?}` is not a URL prefix — write it as \
                             `Fetch \"https://host/path\"`. A fetch grant covers every URL \
                             beginning with the string given."
                        )));
                    }
                    fetch.push(prefix);
                }
                other => return Err(SpecError::UnsupportedCapability(other.to_string())),
            }
            if self.peek() == Some(&Tok::Comma) {
                self.at += 1;
            } else {
                break;
            }
        }
        self.expect(&Tok::CloseBracket)?;
        Ok((roots, fetch))
    }
}
