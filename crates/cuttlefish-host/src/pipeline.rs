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

use cuttlefish_abi::{Signature, Ty};
use std::path::{Path, PathBuf};
use wasmtime::Engine;

/// One stage of a checked pipeline.
pub struct Stage {
    /// Where the block came from, for error messages.
    pub path: PathBuf,
    /// The compiled module.
    pub module_bytes: Vec<u8>,
    /// What it declared.
    pub signature: Signature,
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
    /// A block's module could not be loaded or did not report a signature.
    #[error("inspecting block {path}: {message}")]
    Uninspectable {
        /// The block that could not be inspected.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
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

/// Load a pipeline's blocks and check that each seam fits.
///
/// Fails on the *first* mismatch rather than collecting all of them. A later
/// seam's types depend on an earlier one being what it claimed, so reporting
/// downstream mismatches after an upstream failure would mostly report
/// consequences of the first error rather than independent problems.
pub fn check(engine: &Engine, blocks: &[PathBuf]) -> Result<Checked, PipelineError> {
    if blocks.is_empty() {
        return Err(PipelineError::Empty);
    }

    let mut stages: Vec<Stage> = Vec::with_capacity(blocks.len());
    for path in blocks {
        let module_bytes = std::fs::read(path).map_err(|source| PipelineError::Unreadable {
            path: path.clone(),
            source,
        })?;
        let signature = crate::runner::read_signature(engine, &module_bytes).map_err(|e| {
            PipelineError::Uninspectable {
                path: path.clone(),
                message: e.to_string(),
            }
        })?;

        if let Some(previous) = stages.last() {
            if !previous.signature.output.assignable_to(&signature.input) {
                return Err(PipelineError::SeamMismatch {
                    producer: name_of(&previous.path),
                    produced: previous.signature.output.to_string(),
                    consumer: name_of(path),
                    expected: signature.input.to_string(),
                });
            }
        }

        stages.push(Stage {
            path: path.clone(),
            module_bytes,
            signature,
        });
    }

    Ok(Checked { stages })
}

/// A short name for error messages — the file stem, or the whole path when
/// there isn't one.
fn name_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
