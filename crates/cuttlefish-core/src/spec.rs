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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRef {
    /// A path on this machine, pre-provisioned by the operator.
    Path(String),
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
    /// A model kind that exists in the design but not in this build.
    #[error("unsupported model kind `{0}` (this build supports only `Path`)")]
    UnsupportedModel(String),
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
                let (kind, rest) = value
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| SpecError::Malformed("model needs a kind and a value".into()))?;
                match kind {
                    "Path" => model = Some(ModelRef::Path(quoted(rest, "model")?)),
                    other => return Err(SpecError::UnsupportedModel(other.to_string())),
                }
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
