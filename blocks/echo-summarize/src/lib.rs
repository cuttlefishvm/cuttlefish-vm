//! Example proc-block: summarizes the beginning of a local file.
//!
//! This is a fixture as much as a sample. The host's integration tests build it
//! to `wasm32-unknown-unknown` and drive it through a real wasm boundary, which
//! is what makes those tests exercise the actual ABI rather than a mock of it.
//!
//! Note what this block never does: hold the file. It sees a handle and a
//! length, then decides for itself how much to pull into its own memory. A block
//! wanting the whole file would loop on `Slice` from `next_offset`, and its
//! memory ceiling would still be `WINDOW` rather than the file's size.

use cuttlefish_sdk::{export_block, Block, Command, Event, MediaKind, Signature, TokenAction};

/// Build the summarization prompt.
///
/// The obvious phrasing — "Summarize the following:" then the text — reads to a
/// small model as an assertion to evaluate rather than content to condense.
/// Given a one-line document, llama3.2:1b answered "There is no information
/// about cuttlefish delegating jobs to local models in my knowledge base",
/// which looks like a broken pipeline and is not: the text arrived intact.
///
/// Delimiting the document and saying explicitly not to judge it fixes that
/// with the same model. Prompt shape is part of a block's behaviour, not an
/// afterthought.
fn summarize_prompt(text: &str) -> String {
    format!(
        "Summarize the document between the markers in one or two sentences. \
         Do not comment on whether it is true.\n\n<<<DOCUMENT\n{}\nDOCUMENT>>>",
        text.trim()
    )
}

/// How much of a file this block is willing to hold at once.
///
/// The particular value is arbitrary; the point is that a bound exists. Guest
/// memory is governed by this rather than by the size of the input.
const WINDOW: u64 = 64 * 1024;

#[derive(Default)]
struct EchoSummarize {
    path: String,
    /// Set from the job input purely so the host's early-stop path has a block
    /// that exercises it. A real block would decide from the content it sees.
    stop_after_first: bool,
    seen_tokens: u32,
}

impl Block for EchoSummarize {
    fn signature() -> Signature {
        // Takes a path, produces that path alongside a summary. Stated
        // concretely rather than as `json` so a pipeline seam involving this
        // block is actually checked — a `json` seam typechecks unconditionally,
        // which is the same as not checking it.
        Signature {
            input: "{path: text}".parse().expect("a literal type"),
            output: "{path: text, summary: text}"
                .parse()
                .expect("a literal type"),
        }
    }

    fn start(&mut self, input: serde_json::Value) -> Command {
        self.stop_after_first = input
            .get("stop_after_first")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match input.get("path").and_then(|v| v.as_str()) {
            Some(path) => {
                self.path = path.to_string();
                Command::Open {
                    path: path.to_string(),
                }
            }
            // Fail rather than panic: a panic becomes an opaque wasm trap, while
            // this reaches the caller as a code they can act on.
            None => Command::Fail {
                code: "schema_validation_failed".into(),
                message: "input must have a string `path` field".into(),
            },
        }
    }

    fn step(&mut self, event: Event) -> Command {
        match event {
            // Branch on what the host actually found, rather than assuming
            // text. A PDF gets read through its text layer when it has one, and
            // rendered for a vision model when it does not — which is the
            // difference between summarizing a scan and summarizing nothing.
            Event::Opened { handle, len, kind } => match kind {
                MediaKind::Document {
                    has_text_layer: true,
                    ..
                } => Command::PageText { handle, page: 0 },
                MediaKind::Document { .. } => Command::PageImage { handle, page: 0 },
                MediaKind::Image { .. } => Command::Infer {
                    prompt: "Describe this image.".into(),
                    max_tokens: 128,
                    images: vec![handle],
                },
                MediaKind::Text => Command::Slice {
                    handle,
                    offset: 0,
                    len: len.min(WINDOW),
                },
                MediaKind::Binary => Command::Fail {
                    code: "unsupported".into(),
                    message: "this block reads text, images, and documents".into(),
                },
            },
            Event::Sliced { text, .. } => Command::Infer {
                prompt: summarize_prompt(&text),
                max_tokens: 128,
                images: Vec::new(),
            },
            Event::PageTexted { text } => Command::Infer {
                prompt: summarize_prompt(&text),
                max_tokens: 128,
                images: Vec::new(),
            },
            // A rendered page is an image handle like any other.
            Event::PageImaged { handle, .. } => Command::Infer {
                prompt: "Read this page and summarize it.".into(),
                max_tokens: 128,
                images: vec![handle],
            },
            Event::InferDone { text, .. } => Command::Done {
                result: serde_json::json!({ "path": self.path, "summary": text }),
            },
            Event::SlicedBytes { .. } | Event::Emitted | Event::Embedded { .. } => Command::Fail {
                code: "unexpected_event".into(),
                message: "this block requests neither raw bytes, progress, nor embeddings".into(),
            },
        }
    }

    fn on_token(&mut self, _token: &str) -> TokenAction {
        self.seen_tokens += 1;
        if self.stop_after_first {
            TokenAction::Stop
        } else {
            TokenAction::Continue
        }
    }
}

export_block!(EchoSummarize);
