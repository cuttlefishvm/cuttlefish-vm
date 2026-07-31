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

use cuttlefish_sdk::{export_block, Block, Command, Event, TokenAction};

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
            Event::Opened { handle, len } => Command::Slice {
                handle,
                offset: 0,
                len: len.min(WINDOW),
            },
            Event::Sliced { text, .. } => Command::Infer {
                prompt: format!("Summarize the following:\n\n{text}"),
                max_tokens: 128,
            },
            Event::InferDone { text, .. } => Command::Done {
                result: serde_json::json!({ "path": self.path, "summary": text }),
            },
            Event::Emitted => Command::Fail {
                code: "unexpected_event".into(),
                message: "this block never emits progress".into(),
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
