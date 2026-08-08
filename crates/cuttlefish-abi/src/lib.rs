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

/// The shape of a value flowing through a pipeline.
///
/// Deliberately small. This exists to catch the mistake that actually happens
/// when blocks are composed — one block emitting a summary string into another
/// expecting a list of chunks — not to be a general-purpose type system. A
/// richer one would need inference, and inference over a language with no
/// expressions is machinery without a use.
/// Written and read as a compact string — `text`, `[text]`, `{path: text}` —
/// rather than as a nested tagged object.
///
/// Two reasons, and the second is the one that forced it. It reads well in an
/// error message and in a spec, so one syntax serves the wire, the diagnostics,
/// and the DSL. And a recursive enum serialized structurally makes serde's
/// generic serializer recurse deeply enough to blow rustc's recursion limit in
/// the *guest* crate — which would have meant every block author adding
/// `#![recursion_limit]` to work around a detail of this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    /// A UTF-8 string.
    Text,
    /// Opaque bytes, base64-encoded on the wire.
    Bytes,
    /// A handle naming an image the host holds.
    Image,
    /// A handle naming a paged document.
    Document,
    /// Any JSON value. The top type: everything is assignable to it.
    ///
    /// An escape hatch, and one worth using sparingly — a pipeline of `Json`
    /// seams typechecks unconditionally, which is the same as not checking.
    Json,
    /// An ordered sequence.
    List(Box<Ty>),
    /// A fixed set of named fields.
    ///
    /// A `BTreeMap` so that two records written in different field orders are
    /// the same type, and so error messages list fields the same way twice.
    Record(std::collections::BTreeMap<String, Ty>),
}

impl Ty {
    /// Whether a value of this type can be fed where `expected` is required.
    ///
    /// Not equality: [`Ty::Json`] accepts anything, and a record with *extra*
    /// fields satisfies one that needs fewer. Both directions of that matter —
    /// a block that adds a field should not break its consumer, and a block
    /// that requires a field its producer never emits should fail loudly.
    pub fn assignable_to(&self, expected: &Ty) -> bool {
        match (self, expected) {
            (_, Ty::Json) => true,
            (Ty::List(a), Ty::List(b)) => a.assignable_to(b),
            (Ty::Record(have), Ty::Record(need)) => need
                .iter()
                .all(|(name, want)| have.get(name).is_some_and(|got| got.assignable_to(want))),
            (a, b) => a == b,
        }
    }

    /// Whether a live JSON value could plausibly be an instance of this
    /// type — a runtime counterpart to [`Self::assignable_to`], which only
    /// ever compares two declared `Ty`s against each other. Nothing in the
    /// host checked a block's *actual* output against what it declared
    /// until this existed: a block could claim `{summary: text}` and return
    /// `{text: "..."}` and nothing downstream would notice until whatever
    /// consumed `summary` got `null`.
    ///
    /// Deliberately permissive, not a full validator: [`Ty::Bytes`],
    /// [`Ty::Image`], and [`Ty::Document`] have no fixed JSON shape defined
    /// anywhere in this protocol (unlike [`Ty::Text`]/[`Ty::List`]/
    /// [`Ty::Record`], which map onto JSON strings/arrays/objects
    /// unambiguously) — inventing a shape for them here risks rejecting
    /// legitimate values a real block already produces. Those three, like
    /// [`Ty::Json`], accept anything.
    pub fn matches_value(&self, value: &serde_json::Value) -> bool {
        match self {
            Ty::Json | Ty::Bytes | Ty::Image | Ty::Document => true,
            Ty::Text => value.is_string(),
            Ty::List(inner) => value
                .as_array()
                .is_some_and(|items| items.iter().all(|v| inner.matches_value(v))),
            Ty::Record(fields) => value.as_object().is_some_and(|obj| {
                fields
                    .iter()
                    .all(|(name, want)| obj.get(name).is_some_and(|v| want.matches_value(v)))
            }),
        }
    }

    /// A short human-readable rendering, for error messages.
    pub fn describe(&self) -> String {
        match self {
            Ty::Text => "text".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Image => "image".into(),
            Ty::Document => "document".into(),
            Ty::Json => "json".into(),
            Ty::List(inner) => format!("[{}]", inner.describe()),
            Ty::Record(fields) => {
                let body = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.describe()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{body}}}")
            }
        }
    }
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl std::str::FromStr for Ty {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_ty(s.trim())
    }
}

impl Serialize for Ty {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.describe())
    }
}

impl<'de> Deserialize<'de> for Ty {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Parse the type syntax [`Ty::describe`] produces.
fn parse_ty(s: &str) -> Result<Ty, String> {
    let s = s.trim();
    match s {
        "text" => return Ok(Ty::Text),
        "bytes" => return Ok(Ty::Bytes),
        "image" => return Ok(Ty::Image),
        "document" => return Ok(Ty::Document),
        "json" => return Ok(Ty::Json),
        _ => {}
    }

    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Ok(Ty::List(Box::new(parse_ty(inner)?)));
    }

    if let Some(body) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let mut fields = std::collections::BTreeMap::new();
        if !body.trim().is_empty() {
            for part in split_fields(body) {
                let (name, ty) = part
                    .split_once(':')
                    .ok_or_else(|| format!("expected `name: type` in `{part}`"))?;
                fields.insert(name.trim().to_string(), parse_ty(ty)?);
            }
        }
        return Ok(Ty::Record(fields));
    }

    Err(format!("`{s}` is not a type"))
}

/// Split record fields on commas that are not inside a nested `[]` or `{}`.
///
/// A plain `split(',')` would cut `{a: [x, y]}` in the wrong place.
fn split_fields(body: &str) -> Vec<String> {
    let (mut out, mut depth, mut current) = (Vec::new(), 0i32, String::new());
    for c in body.chars() {
        match c {
            '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// What a block accepts and produces.
///
/// Declared by the block itself, through a `cf_signature` export, rather than in
/// a sidecar file beside it. A sidecar can disagree with the code it describes
/// and nothing forces anyone to notice; a declaration compiled into the module
/// travels with it, cannot go stale, and leaves one artifact to ship rather than
/// two to keep in step.
///
/// `Display`/`FromStr` render and parse it as `"{input} -> {output}"` —
/// each side is a [`Ty`], and this is the compact form the catalog caches
/// and a bundle manifest embeds. Splitting on `" -> "` is unambiguous only
/// because `Ty::describe()` never produces that substring itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// What the block needs as input.
    pub input: Ty,
    /// What it produces.
    pub output: Ty,
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}", self.input, self.output)
    }
}

impl std::str::FromStr for Signature {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (input, output) = s
            .split_once(" -> ")
            .ok_or_else(|| format!("`{s}` is not a signature (expected `input -> output`)"))?;
        Ok(Signature {
            input: input.parse()?,
            output: output.parse()?,
        })
    }
}

/// What kind of thing a handle refers to, reported by [`Event::Opened`].
///
/// A block needs this to know which commands are worth issuing: [`Command::Slice`]
/// on a PNG is a mistake, and [`Command::PageText`] on a plain text file is
/// meaningless. Reporting it up front means a block can branch on what it
/// actually got rather than guessing from a file extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaKind {
    /// Valid UTF-8. Both [`Command::Slice`] and [`Command::SliceBytes`] work.
    Text,
    /// An image the host recognised. Usable as an [`Command::Infer`] image.
    Image {
        /// Format as detected from content, e.g. `png`, `jpeg`.
        format: String,
    },
    /// A paged document — a PDF, say.
    Document {
        /// How many pages it has.
        pages: u32,
        /// Whether it carries an extractable text layer.
        ///
        /// False for a scanned document, where the only way to read it is to
        /// rasterize pages and hand them to a vision model. A block that checks
        /// this can pick the cheap path when it exists and the expensive one
        /// when it must, instead of silently extracting nothing.
        has_text_layer: bool,
    },
    /// Bytes the host could not classify. Only [`Command::SliceBytes`] applies.
    Binary,
}

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
        /// Images to accompany the prompt, named by handle.
        ///
        /// Handles rather than bytes, for the same reason file contents are not
        /// handed over: an image can be tens of megabytes, and routing it
        /// through guest memory would put the 4 GiB wasm32 ceiling back in play
        /// for no benefit. The host already holds the bytes; it can pass them to
        /// the model directly.
        ///
        /// Requires a model with vision capability. Empty for ordinary
        /// text-only inference, which is why it is `#[serde(default)]` — a block
        /// compiled before this field existed still deserializes.
        #[serde(default)]
        images: Vec<Handle>,
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
    /// Pull one bounded window of an open file as raw bytes.
    ///
    /// The binary counterpart to [`Command::Slice`]. Prefer `Slice` for text:
    /// it needs no encoding, and it handles the character-boundary problem for
    /// you. This exists for blocks that genuinely need bytes — inspecting an
    /// image header, say — and pays base64's cost to carry them.
    SliceBytes {
        /// Handle from a previous [`Command::Open`].
        handle: Handle,
        /// Byte offset to read from.
        offset: u64,
        /// Maximum bytes to return.
        len: u64,
    },
    /// Extract one page of a document as text.
    ///
    /// Fails when the document has no text layer; check
    /// [`MediaKind::Document::has_text_layer`] first.
    PageText {
        /// Handle from a previous [`Command::Open`].
        handle: Handle,
        /// Zero-based page number.
        page: u32,
    },
    /// Render one page of a document to an image.
    ///
    /// Yields a *new* handle referring to the rendered image, which can then be
    /// named in [`Command::Infer`]. That indirection is deliberate: the image
    /// stays host-side like every other bulk value, and a rendered page is
    /// usable exactly wherever a file-backed image is.
    PageImage {
        /// Handle from a previous [`Command::Open`].
        handle: Handle,
        /// Zero-based page number.
        page: u32,
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
        /// What the host made of the contents; see [`MediaKind`].
        ///
        /// `#[serde(default)]` so a block built before this field existed still
        /// deserializes, treating anything it opens as text.
        #[serde(default)]
        kind: MediaKind,
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
    /// A window of a file was read as raw bytes.
    SlicedBytes {
        /// The window's contents, base64-encoded.
        ///
        /// Base64 rather than a binary side channel: the boundary is JSON, and
        /// keeping it inspectable is worth more than the third it costs on a
        /// path blocks are not expected to use in bulk.
        bytes_base64: String,
        /// Where the returned bytes ended. Unlike [`Event::Sliced`] there is no
        /// truncation, so this is always `offset + len` clamped to the file.
        next_offset: u64,
    },
    /// A document page was extracted as text.
    PageTexted {
        /// The page's text.
        text: String,
    },
    /// A document page was rendered to an image.
    PageImaged {
        /// A new handle referring to the rendered image; name it in
        /// [`Command::Infer`].
        handle: Handle,
        /// Its size in bytes.
        len: u64,
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
    /// Was running when the daemon last stopped; not resumed automatically.
    Interrupted,
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
    /// A Script-kind block's own logic failed at run time — a Rhai runtime
    /// error (e.g. an out-of-bounds index, a missing function) surfacing
    /// from inside the script itself. `catalog add` rejects a script whose
    /// body doesn't *parse*, so this is specifically a failure that only
    /// shows up once the script actually runs; it is not a schema mismatch,
    /// so it does not reuse [`SCHEMA_VALIDATION_FAILED`].
    pub const SCRIPT_ERROR: &str = "script_error";
    /// The guest trapped — a panic, a bad export signature, or malformed wasm.
    pub const WASM_TRAP: &str = "wasm_trap";
    /// The job exceeded its time budget.
    pub const TIMEOUT: &str = "timeout";
    /// The job was cancelled by request.
    pub const CANCELLED: &str = "cancelled";
    /// A command needed a capability this build does not have — asking for a
    /// page image without document rendering compiled in, say.
    pub const UNSUPPORTED: &str = "unsupported";
}

impl Default for MediaKind {
    /// Text, because that is what every command predating [`MediaKind`]
    /// assumed, and because it keeps an older block's behaviour unchanged.
    fn default() -> Self {
        Self::Text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_round_trips_through_its_compact_string() {
        let sig = Signature {
            input: Ty::Record([("path".to_string(), Ty::Text)].into_iter().collect()),
            output: Ty::List(Box::new(Ty::Text)),
        };
        let s = sig.to_string();
        assert_eq!(s, "{path: text} -> [text]");
        assert_eq!(s.parse::<Signature>().unwrap(), sig);
    }

    #[test]
    fn a_signature_without_an_arrow_is_rejected() {
        assert!("just-a-type".parse::<Signature>().is_err());
    }

    #[test]
    fn a_signature_with_an_unparseable_side_is_rejected() {
        assert!("text -> not a type".parse::<Signature>().is_err());
    }

    #[test]
    fn text_matches_a_json_string_only() {
        assert!(Ty::Text.matches_value(&serde_json::json!("hello")));
        assert!(!Ty::Text.matches_value(&serde_json::json!(42)));
    }

    #[test]
    fn json_matches_anything() {
        assert!(Ty::Json.matches_value(&serde_json::json!(null)));
        assert!(Ty::Json.matches_value(&serde_json::json!([1, "two", {}])));
    }

    #[test]
    fn a_record_missing_a_declared_field_does_not_match() {
        let ty = Ty::Record([("summary".to_string(), Ty::Text)].into_iter().collect());
        assert!(!ty.matches_value(&serde_json::json!({"text": "hello world"})));
    }

    #[test]
    fn a_record_with_the_declared_field_present_and_well_typed_matches() {
        let ty = Ty::Record([("summary".to_string(), Ty::Text)].into_iter().collect());
        assert!(ty.matches_value(&serde_json::json!({"summary": "hi", "extra": 1})));
    }

    #[test]
    fn a_record_with_a_wrong_typed_declared_field_does_not_match() {
        let ty = Ty::Record([("summary".to_string(), Ty::Text)].into_iter().collect());
        assert!(!ty.matches_value(&serde_json::json!({"summary": 42})));
    }

    #[test]
    fn a_list_matches_only_when_every_element_matches_the_inner_type() {
        let ty = Ty::List(Box::new(Ty::Text));
        assert!(ty.matches_value(&serde_json::json!(["a", "b"])));
        assert!(!ty.matches_value(&serde_json::json!(["a", 2])));
        assert!(!ty.matches_value(&serde_json::json!("not a list")));
    }

    #[test]
    fn bytes_image_and_document_accept_anything() {
        for ty in [Ty::Bytes, Ty::Image, Ty::Document] {
            assert!(ty.matches_value(&serde_json::json!(123)));
        }
    }
}
