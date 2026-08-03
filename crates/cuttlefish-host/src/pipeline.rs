//! Checking that a pipeline's blocks fit together, before any of them run.
//!
//! A pipeline feeds each block's output into the next one's input. Those seams
//! are where composition actually goes wrong: a block producing a summary string
//! handed to one expecting a list of chunks does not fail at the seam — it fails
//! somewhere inside the second block, as a confusing error about a field that is
//! missing, or worse, as a plausible answer computed from nothing.
//!
//! So the seams are checked first, using signatures the blocks declare
//! themselves (see [`crate::runner::read_signature`]). A mismatch names both
//! blocks and both types and stops the job before it starts.
//!
//! # What this deliberately is not
//!
//! Not type *inference*. Every block states its own signature; nothing is
//! derived. Inference earns its keep in a language with expressions, where types
//! flow through terms nobody wants to annotate. A pipeline is a list of already
//! typed things, so there is nothing to infer and machinery to infer it would be
//! cost without benefit.
//!
//! Not a DAG, yet. A pipeline is linear. Branching and joining are real needs,
//! but a linear chain covers the pipelines that exist today, and the type
//! discipline established here is what a DAG would extend rather than replace.

use crate::catalog::{Catalog, ResolutionContext, Resolved};
use cuttlefish_abi::{Signature, Ty};
use std::path::{Path, PathBuf};
use wasmtime::Engine;

/// One stage of a checked pipeline.
pub struct Stage {
    /// Display name — a file stem for a path-resolved stage, or the bare
    /// catalog name (no `@version`) for a cataloged one. Used only for
    /// output and the bundle manifest, never round-tripped into a lookup.
    pub name: String,
    /// Block or bundle.
    pub kind: crate::catalog::ArtifactKind,
    /// The exact `name@version` this stage resolved to, if it came from the
    /// catalog. `None` for a direct path.
    pub resolved: Option<String>,
    /// The compiled module (block) or `.cfbundle` (bundle) bytes.
    pub module_bytes: Vec<u8>,
    /// What it declared.
    pub signature: Signature,
}

/// One pipeline entry's bytes, already resolved and loaded — from disk or
/// from the catalog's blob store. `check()`'s input; see resolve_and_load
/// (added in a later change) for how one of these gets built.
pub struct ResolvedInput {
    /// Display name, same convention as [`Stage::name`].
    pub name: String,
    /// Block or bundle, already determined (either sniffed from a direct
    /// path's magic bytes, or read from the catalog entry's own `kind`).
    pub kind: crate::catalog::ArtifactKind,
    /// The exact `name@version`, if this came from the catalog.
    pub resolved: Option<String>,
    /// The raw bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ResolvedInput {
    /// Hand-written so a `Debug`-formatted `ResolvedInput` (e.g. from a
    /// panic or failed assertion) never dumps a multi-megabyte wasm module
    /// as a wall of numbers — `bytes` is reported by length only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedInput")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("resolved", &self.resolved)
            .field("bytes", &format!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// A pipeline whose seams have been checked.
///
/// Only constructible through [`check`], so holding one is evidence the check
/// ran — a type that cannot be built in an unchecked state is worth more than a
/// convention that it should not be.
pub struct Checked {
    stages: Vec<Stage>,
}

impl Checked {
    /// The stages, in execution order.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// What the pipeline as a whole accepts — its first block's input.
    pub fn input(&self) -> &Ty {
        &self.stages[0].signature.input
    }

    /// What it produces — its last block's output.
    pub fn output(&self) -> &Ty {
        &self.stages[self.stages.len() - 1].signature.output
    }
}

/// Why a pipeline was rejected.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// A block could not be read from disk.
    #[error("reading block {path}: {source}")]
    Unreadable {
        /// The block that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// A stage could not be inspected: a directory instead of an artifact,
    /// unrecognized magic bytes, a wasm module that failed to load, or a
    /// bundle whose cached signature string doesn't parse.
    #[error("inspecting {name}: {message}")]
    Uninspectable {
        /// The stage's display name.
        name: String,
        /// What went wrong.
        message: String,
    },
    /// Resolving a pipeline entry through the catalog failed.
    #[error(transparent)]
    Resolution(#[from] crate::catalog::CatalogError),
    /// Two adjacent blocks do not fit.
    #[error(
        "block {consumer} expects {expected}, but {producer} before it produces {produced}.\n\
         Adjust one of the two signatures, or insert a block that converts between them."
    )]
    SeamMismatch {
        /// The block producing the value.
        producer: String,
        /// What it produces.
        produced: String,
        /// The block receiving it.
        consumer: String,
        /// What it needs.
        expected: String,
    },
    /// The pipeline had no blocks.
    #[error("a pipeline needs at least one block")]
    Empty,
}

/// Check that a resolved pipeline's seams fit.
///
/// Fails on the *first* mismatch rather than collecting all of them. A later
/// seam's types depend on an earlier one being what it claimed, so reporting
/// downstream mismatches after an upstream failure would mostly report
/// consequences of the first error rather than independent problems.
///
/// Takes already-resolved, already-loaded stages — see resolve_and_load
/// (added in a later change) for turning a spec's pipeline entries into
/// these. `check` itself does no disk or catalog I/O; a `Direct` vs.
/// `Cataloged` entry looks identical to it once loaded.
pub fn check(engine: &Engine, inputs: &[ResolvedInput]) -> Result<Checked, PipelineError> {
    if inputs.is_empty() {
        return Err(PipelineError::Empty);
    }

    let mut stages: Vec<Stage> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let signature = match input.kind {
            crate::catalog::ArtifactKind::Block => {
                crate::runner::read_signature(engine, &input.bytes).map_err(|e| {
                    PipelineError::Uninspectable {
                        name: input.name.clone(),
                        message: format!("{e:#}"),
                    }
                })?
            }
            crate::catalog::ArtifactKind::Bundle => {
                let compact = crate::catalog::read_bundle_signature(&input.bytes, &input.name)
                    .map_err(|e| PipelineError::Uninspectable {
                        name: input.name.clone(),
                        // `read_bundle_signature`'s own error already embeds
                        // the name (it's `{path}: {reason}` with `path` set
                        // to our `name`); re-stringifying the whole error
                        // here would print the name twice. Pull out just the
                        // reason so `Uninspectable`'s own `{name}: {message}`
                        // formatting is the only place the name appears.
                        message: match e {
                            crate::catalog::CatalogError::UninspectableArtifact {
                                reason, ..
                            } => reason,
                            other => other.to_string(),
                        },
                    })?;
                compact
                    .parse::<Signature>()
                    .map_err(|e| PipelineError::Uninspectable {
                        name: input.name.clone(),
                        message: format!("cached signature `{compact}` does not parse: {e}"),
                    })?
            }
        };

        if let Some(previous) = stages.last() {
            if !previous.signature.output.assignable_to(&signature.input) {
                return Err(PipelineError::SeamMismatch {
                    producer: previous.name.clone(),
                    produced: previous.signature.output.to_string(),
                    consumer: input.name.clone(),
                    expected: signature.input.to_string(),
                });
            }
        }

        stages.push(Stage {
            name: input.name.clone(),
            kind: input.kind,
            resolved: input.resolved.clone(),
            module_bytes: input.bytes.clone(),
            signature,
        });
    }

    Ok(Checked { stages })
}

/// Turn one pipeline-entry string from a spec into a loaded, kind-tagged
/// [`ResolvedInput`], ready for [`check`]. Shared by `cuttlefishd`'s run
/// path and `cuttlefish build` — the only two callers, and both need the
/// same resolve-then-load behavior.
///
/// An entry that resolves to a real path once joined against `spec_dir` is
/// treated as that path — matching the join every other spec-relative
/// reference (`capabilities`) already gets, so a relative block path means
/// the same thing regardless of the process's working directory. This
/// mirrors [`Catalog::resolve`]'s own Direct-vs-Cataloged decision (`s.ends_with(".wasm")
/// || Path::new(s).exists()`), just relative to `spec_dir` instead of the
/// process's CWD, rather than guessing from the raw string's shape (`entry`
/// containing `/`) before `resolve` ever gets a look — a catalog name is
/// free to contain `/` (a namespaced convention like `"team/cat-a@1"`), and
/// joining on shape alone would corrupt such a name into a bogus path before
/// the catalog's own index lookup ever sees it.
///
/// A `.wasm`/`.cfbundle` suffix is still always treated as a path reference
/// even when the joined candidate doesn't exist on disk, so a genuinely
/// missing compiled artifact still names the expected file in its error
/// instead of silently falling back to a catalog lookup on a string that
/// happens to contain that suffix. Anything else that doesn't resolve to a
/// real file is passed to [`Catalog::resolve`] unmodified — joining it would
/// turn `name@version` into `<spec_dir>/name@version`, which matches neither
/// an index key nor a real file.
pub fn resolve_and_load(
    catalog: &Catalog,
    spec_dir: &Path,
    entry: &str,
    context: ResolutionContext,
) -> Result<ResolvedInput, PipelineError> {
    let candidate = spec_dir.join(entry);
    let use_joined = candidate.exists() || entry.ends_with(".wasm") || entry.ends_with(".cfbundle");
    let joined;
    let s: &str = if use_joined {
        joined = candidate.to_string_lossy().into_owned();
        &joined
    } else {
        entry
    };

    match catalog.resolve(s, context)? {
        Resolved::Direct(path) => {
            if path.is_dir() {
                return Err(PipelineError::Uninspectable {
                    name: path.display().to_string(),
                    message: "this is a directory. A pipeline names compiled \
                              `.wasm`/`.cfbundle` artifacts, not block source \
                              directories — build the block first and point at \
                              the compiled artifact."
                        .into(),
                });
            }
            let bytes = std::fs::read(&path).map_err(|source| PipelineError::Unreadable {
                path: path.clone(),
                source,
            })?;
            let kind = crate::catalog::sniff_artifact_kind(&bytes).ok_or_else(|| {
                PipelineError::Uninspectable {
                    name: path.display().to_string(),
                    message: "not a recognized artifact (neither wasm nor .cfbundle magic \
                              bytes)"
                        .into(),
                }
            })?;
            Ok(ResolvedInput {
                name: name_of(&path),
                kind,
                resolved: None,
                bytes,
            })
        }
        Resolved::Cataloged {
            name_version,
            entry,
        } => {
            let bytes = catalog.read_blob(&entry)?;
            let name = name_version
                .split_once('@')
                .map(|(n, _)| n)
                .unwrap_or(&name_version)
                .to_string();
            Ok(ResolvedInput {
                name,
                kind: entry.kind,
                resolved: Some(name_version),
                bytes,
            })
        }
    }
}

/// A short name for error messages and manifest output — the file stem, or
/// the whole path when there isn't one (`Path::file_stem(".wasm")` returns
/// `Some(".wasm")` under the leading-dot rule, so this never panics or
/// empties out on a no-real-basename path).
fn name_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
