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
    /// The proc-block implementing the job.
    pub block: PathBuf,
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

/// Strip surrounding double quotes, or explain that they were required.
fn quoted(value: &str, field: &str) -> Result<String, SpecError> {
    value
        .trim()
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(str::to_string)
        .ok_or_else(|| SpecError::Malformed(format!("field `{field}` must be a quoted string")))
}

/// Parse the capability list: `[ Read "a", Read "b" ]`.
fn capabilities(value: &str) -> Result<Vec<PathBuf>, SpecError> {
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| SpecError::Malformed("capabilities must be a `[...]` list".into()))?;

    inner
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let rest = entry.strip_prefix("Read ").ok_or_else(|| {
                // Name the offending kind rather than saying "invalid": the
                // author needs to know which entry, and this list is one place
                // a typo grants nothing while looking correct.
                let kind = entry.split_whitespace().next().unwrap_or(entry);
                SpecError::UnsupportedCapability(kind.to_string())
            })?;
            Ok(PathBuf::from(quoted(rest, "capabilities")?))
        })
        .collect()
}

/// Parse a spec.
pub fn parse_spec(src: &str) -> Result<Spec, SpecError> {
    let open = src
        .find('{')
        .ok_or_else(|| SpecError::Malformed("expected `{`".into()))?;
    let close = src
        .rfind('}')
        .ok_or_else(|| SpecError::Malformed("expected `}`".into()))?;
    if close < open {
        return Err(SpecError::Malformed("`}` before `{`".into()));
    }

    let name = src[..open]
        .trim()
        .strip_prefix("spec")
        .and_then(|header| header.split('=').next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| SpecError::Malformed("expected `spec <name> = {`".into()))?
        .to_string();

    let (mut description, mut model, mut data_policy, mut read_roots, mut block) =
        (None, None, None, None, None);

    for statement in src[open + 1..close].split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }

        let (key, value) = statement.split_once('=').ok_or_else(|| {
            SpecError::Malformed(format!("expected `key = value` in `{statement}`"))
        })?;
        let value = value.trim();

        match key.trim() {
            "description" => description = Some(quoted(value, "description")?),
            "block" => block = Some(PathBuf::from(quoted(value, "block")?)),
            "capabilities" => read_roots = Some(capabilities(value)?),
            "model" => {
                // Any `Provider "target"` parses. Whether that provider exists
                // is the host's question, not this parser's — see `ModelRef`.
                let (provider, rest) = value.split_once(char::is_whitespace).ok_or_else(|| {
                    SpecError::Malformed(
                        r#"model needs a provider and a target, as in `Ollama "llama3.2:1b"`"#
                            .into(),
                    )
                })?;
                if provider.is_empty() || !provider.chars().all(|c| c.is_alphanumeric() || c == '_')
                {
                    return Err(SpecError::Malformed(format!(
                        "`{provider}` is not a valid model provider name"
                    )));
                }
                model = Some(ModelRef::new(provider, quoted(rest, "model")?));
            }
            "data_policy" => {
                data_policy = Some(match value {
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
    }

    Ok(Spec {
        name,
        description: description.ok_or(SpecError::MissingField("description"))?,
        model: model.ok_or(SpecError::MissingField("model"))?,
        data_policy: data_policy.ok_or(SpecError::MissingField("data_policy"))?,
        read_roots: read_roots.ok_or(SpecError::MissingField("capabilities"))?,
        block: block.ok_or(SpecError::MissingField("block"))?,
    })
}
