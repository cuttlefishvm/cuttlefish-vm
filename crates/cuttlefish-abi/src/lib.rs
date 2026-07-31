//! The contract between the cuttlefish host and its guest proc-blocks.
//!
//! Both sides depend on this crate precisely so that they cannot drift: a block
//! is compiled separately from the host, often at a different time by a
//! different person, and the only thing keeping them able to talk is that they
//! agreed on these types.
//!
//! # Why a command loop, not function calls
//!
//! A block does not call the host. It *returns* a [`Command`] describing what it
//! wants done, and the host — after doing it — hands back an [`Event`] and asks
//! for the next command. Control is inverted relative to the obvious design, and
//! not for taste:
//!
//! - A core-wasm guest is single-threaded and offers no execution context the
//!   host could call back into while the guest is blocked. A "call the host and
//!   wait" design has nowhere to deliver the answer.
//! - Inference must run on a different thread from the wasm store, which is
//!   `!Sync` and cannot be touched from there.
//! - Because the host decides whether to take the next step, cancellation needs
//!   no cooperation from the guest at all: the host simply stops stepping. A
//!   guest cannot ignore, delay, or trap its way out of being cancelled.
//!
//! Everything crosses the boundary as JSON. That is slower than a packed binary
//! layout, deliberately: the boundary stays inspectable, a mismatch produces a
//! legible error rather than a misread integer, and the volume is low because
//! bulk data does not cross it. Revisit only if profiling says to.
//!
//! # Why bulk data does not cross this boundary
//!
//! No command hands a block the contents of a file. A block [`Command::Open`]s a
//! path, receives a [`Handle`] and a length, then pulls bounded windows with
//! [`Command::Slice`].
//!
//! This keeps guest memory proportional to the window a block chooses rather
//! than to the size of its input. A block written against a small file behaves
//! identically against a huge one, and the 4 GiB ceiling of 32-bit wasm stops
//! being something block authors must reason about — which is what lets this
//! project stay on `wasm32` instead of paying for `wasm64`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// A job-scoped reference to something the host holds open for a guest.
///
/// Job-scoping is a security property, not bookkeeping. A handle table lives and
/// dies with a single job, so a handle from one job names nothing in another.
/// That is why [`Command::Slice`] carries no path and needs no capability check
/// of its own: the check happened once, at [`Command::Open`], and a handle
/// cannot be forged into a reference to another job's data.
pub type Handle = u32;

/// What a guest asks the host to do, returned from its `init`/`step` exports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Run a prompt against the job's model.
    Infer {
        /// The prompt to generate from.
        prompt: String,
        /// Upper bound on tokens generated. A guest can also end generation
        /// early by returning [`TokenAction::Stop`] from its `on_token` export.
        max_tokens: u32,
    },
    /// Open a file. Capability-checked against the job's spec.
    ///
    /// Yields a handle and a length rather than contents — see the crate docs on
    /// why bulk data does not cross this boundary.
    Open {
        /// Path to open. Denied unless the spec grants read access to it.
        path: String,
    },
    /// Pull one bounded window of an open file into guest memory.
    ///
    /// The guest picks `len`, so the guest sets its own memory ceiling.
    Slice {
        /// Handle from a previous [`Command::Open`].
        handle: Handle,
        /// Byte offset to read from. `u64` so that files far larger than a guest
        /// could hold remain fully addressable.
        offset: u64,
        /// Maximum bytes to return. The host may return fewer; see
        /// [`Event::Sliced`].
        len: u64,
    },
    /// Report progress to whoever is watching the job's event stream.
    Emit {
        /// Arbitrary JSON, forwarded verbatim to the job's subscribers.
        progress: serde_json::Value,
    },
    /// Finish successfully with this payload.
    Done {
        /// The job's result, shaped by the spec's declared output.
        result: serde_json::Value,
    },
    /// Give up. The job ends with this code and message, and no result.
    Fail {
        /// Machine-readable code; see [`error_codes`].
        code: String,
        /// Human-readable explanation.
        message: String,
    },
}

/// What the host feeds back into the guest's `step` export after carrying out a
/// [`Command`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Generation finished.
    InferDone {
        /// The generated text.
        text: String,
        /// How many tokens were produced. May be fewer than the requested
        /// `max_tokens` if the guest ended generation early.
        tokens_out: u32,
    },
    /// A file was opened.
    Opened {
        /// Use this in subsequent [`Command::Slice`] calls.
        handle: Handle,
        /// Total size of the file, in bytes.
        len: u64,
    },
    /// A window of a file was read.
    Sliced {
        /// The window's contents.
        text: String,
        /// Where the returned text actually ended.
        ///
        /// This is **not** always `offset + len` from the request: the host cuts
        /// a window back to a UTF-8 character boundary, because a caller picking
        /// window sizes has no idea where characters begin, and a naive split
        /// would corrupt a multi-byte character at nearly every seam. A guest
        /// walking a file must resume from this value rather than advancing by
        /// the length it asked for.
        next_offset: u64,
    },
    /// Progress was forwarded. Carries nothing; it exists so `Emit` has a reply
    /// and the command loop keeps its shape.
    Emitted,
}

/// A guest's verdict on each streamed token, returned from its `on_token`
/// export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAction {
    /// Keep generating.
    Continue,
    /// Stop generating now.
    ///
    /// A token or two may still arrive after this, because the verdict has to
    /// travel back to the thread doing the generating.
    Stop,
}

impl TokenAction {
    /// Decode the raw `i32` a guest's `on_token` export returns.
    ///
    /// Anything that is not an explicit `Continue` reads as `Stop`. A guest
    /// returning a value this crate does not recognise is malfunctioning, and
    /// the safe reading of a malfunctioning guest is "stop", never "keep
    /// spending tokens" — the same fail-closed posture as the capability checks.
    pub fn from_i32(v: i32) -> Self {
        if v == 0 {
            Self::Continue
        } else {
            Self::Stop
        }
    }

    /// Encode for the wasm boundary.
    ///
    /// These integers are part of the ABI: renumbering them silently changes the
    /// meaning of every already-compiled block.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Continue => 0,
            Self::Stop => 1,
        }
    }
}

/// Where a job is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Accepted, not yet started.
    Queued,
    /// Executing.
    Running,
    /// Finished with a result.
    Completed,
    /// Finished with an error and no result.
    Failed,
    /// Stopped by request.
    Cancelled,
}

impl JobStatus {
    /// Whether this status is final — nothing further will happen to the job.
    ///
    /// Clients poll until this is true. Adding a new non-terminal status is
    /// therefore safe, while a new terminal one that is missing from this match
    /// leaves callers waiting forever.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// What a job cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// Tokens consumed by prompts.
    pub tokens_in: u32,
    /// Tokens generated.
    pub tokens_out: u32,
    /// Wall-clock duration of the job.
    pub duration_ms: u64,
    /// Which model served the job's inference.
    pub model: String,
}

/// The fixed, spec-independent envelope handed back to the calling agent.
///
/// Every job returns this shape regardless of what it did, so an agent can
/// handle results without knowing anything about the block that produced them.
/// Only `result` varies, and its shape is that job's business.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    /// Lifecycle state; see [`JobStatus::is_terminal`].
    pub status: JobStatus,
    /// Present only when the job completed.
    ///
    /// A failed or cancelled job never carries a partial result: a caller must
    /// never have to guess whether a payload is trustworthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Present only when the job failed or was cancelled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
    /// Cost accounting, populated even for failed jobs — work already spent
    /// still counts.
    pub usage: Usage,
}

/// Why a job did not complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobError {
    /// Machine-readable; see [`error_codes`].
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

/// The error codes the daemon emits in [`JobError::code`].
///
/// These are string constants rather than an enum so the set can grow without
/// breaking clients that match on strings, and so a client built against an
/// older version meets an unfamiliar code rather than a decode failure.
pub mod error_codes {
    /// The job's model could not be loaded or served.
    pub const MODEL_LOAD_FAILED: &str = "model_load_failed";
    /// A guest tried to reach something its spec does not grant.
    pub const CAPABILITY_DENIED: &str = "capability_denied";
    /// Job input did not match the spec's declared shape.
    pub const SCHEMA_VALIDATION_FAILED: &str = "schema_validation_failed";
    /// The guest trapped — a panic, a bad export signature, or malformed wasm.
    pub const WASM_TRAP: &str = "wasm_trap";
    /// The job exceeded its time budget.
    pub const TIMEOUT: &str = "timeout";
    /// The job was cancelled by request.
    pub const CANCELLED: &str = "cancelled";
}
