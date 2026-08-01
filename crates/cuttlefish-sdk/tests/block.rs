//! Tests for the author-facing `Block` trait.
//!
//! These drive a block the way the host will — start it, then step it with the
//! event each command produces — without any wasm involved. That separation is
//! deliberate: it means a block author can unit-test their state machine
//! natively, and only the boundary itself needs a wasm harness.

use cuttlefish_abi::{Command, Event, TokenAction};
use cuttlefish_sdk::Block;

/// A block that walks a file in windows and summarizes the whole thing.
#[derive(Default)]
struct Walker {
    handle: u32,
    len: u64,
    offset: u64,
    collected: String,
    stop_after: Option<u32>,
    tokens_seen: u32,
}

const WINDOW: u64 = 4;

impl Block for Walker {
    fn start(&mut self, input: serde_json::Value) -> Command {
        self.stop_after = input
            .get("stop_after")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        match input.get("path").and_then(|v| v.as_str()) {
            Some(path) => Command::Open {
                path: path.to_string(),
            },
            None => Command::Fail {
                code: cuttlefish_abi::error_codes::SCHEMA_VALIDATION_FAILED.into(),
                message: "input needs a string `path`".into(),
            },
        }
    }

    fn step(&mut self, event: Event) -> Command {
        match event {
            Event::Opened { handle, len, .. } => {
                self.handle = handle;
                self.len = len;
                Command::Slice {
                    handle,
                    offset: 0,
                    len: WINDOW,
                }
            }
            Event::Sliced { text, next_offset } => {
                self.collected.push_str(&text);
                self.offset = next_offset;
                if next_offset < self.len {
                    // Resume from next_offset, not offset + WINDOW — the host
                    // may have returned a short window to avoid splitting a
                    // character.
                    Command::Slice {
                        handle: self.handle,
                        offset: next_offset,
                        len: WINDOW,
                    }
                } else {
                    Command::Infer {
                        prompt: format!("Summarize: {}", self.collected),
                        max_tokens: 16,
                        images: Vec::new(),
                    }
                }
            }
            Event::InferDone { text, .. } => Command::Done {
                result: serde_json::json!({ "summary": text, "read": self.collected }),
            },
            _ => Command::Fail {
                code: "unexpected_event".into(),
                message: "this block handles only open, slice, and infer".into(),
            },
        }
    }

    fn on_token(&mut self, _token: &str) -> TokenAction {
        self.tokens_seen += 1;
        match self.stop_after {
            Some(n) if self.tokens_seen >= n => TokenAction::Stop,
            _ => TokenAction::Continue,
        }
    }
}

#[test]
fn a_block_walks_a_file_in_windows_and_finishes() {
    let mut b = Walker::default();

    assert_eq!(
        b.start(serde_json::json!({"path": "/doc.txt"})),
        Command::Open {
            path: "/doc.txt".into()
        }
    );

    // Host opens a 10-byte file.
    assert_eq!(
        b.step(Event::Opened {
            handle: 1,
            len: 10,
            kind: cuttlefish_abi::MediaKind::Text
        }),
        Command::Slice {
            handle: 1,
            offset: 0,
            len: WINDOW
        }
    );

    // Two full windows, then a short one to reach the end.
    assert_eq!(
        b.step(Event::Sliced {
            text: "abcd".into(),
            next_offset: 4
        }),
        Command::Slice {
            handle: 1,
            offset: 4,
            len: WINDOW
        }
    );
    assert_eq!(
        b.step(Event::Sliced {
            text: "efgh".into(),
            next_offset: 8
        }),
        Command::Slice {
            handle: 1,
            offset: 8,
            len: WINDOW
        }
    );

    let cmd = b.step(Event::Sliced {
        text: "ij".into(),
        next_offset: 10,
    });
    assert_eq!(
        cmd,
        Command::Infer {
            prompt: "Summarize: abcdefghij".into(),
            max_tokens: 16,
            images: Vec::new()
        },
        "reaching the end must move on to inference, not slice past it"
    );

    assert_eq!(
        b.step(Event::InferDone {
            text: "a summary".into(),
            tokens_out: 2
        }),
        Command::Done {
            result: serde_json::json!({"summary": "a summary", "read": "abcdefghij"})
        }
    );
}

#[test]
fn a_short_window_is_resumed_from_next_offset() {
    // The host truncates a window at a character boundary, so next_offset can
    // be less than offset + len. A block that advanced by its own request size
    // would skip bytes; this pins that it does not.
    let mut b = Walker::default();
    b.start(serde_json::json!({"path": "/d"}));
    b.step(Event::Opened {
        handle: 9,
        len: 6,
        kind: cuttlefish_abi::MediaKind::Text,
    });

    let cmd = b.step(Event::Sliced {
        text: "ab".into(),
        next_offset: 2, // short: asked for 4, got 2
    });
    assert_eq!(
        cmd,
        Command::Slice {
            handle: 9,
            offset: 2,
            len: WINDOW
        },
        "must resume from next_offset, not from offset + requested len"
    );
}

#[test]
fn missing_required_input_fails_instead_of_panicking() {
    let mut b = Walker::default();
    let cmd = b.start(serde_json::json!({}));
    assert!(
        matches!(cmd, Command::Fail { ref code, .. }
            if code == cuttlefish_abi::error_codes::SCHEMA_VALIDATION_FAILED),
        "got {cmd:?}"
    );
}

#[test]
fn on_token_defaults_to_continue() {
    // A block that does not care about streaming must not have to implement
    // on_token, and the default must never accidentally stop generation.
    #[derive(Default)]
    struct Minimal;
    impl Block for Minimal {
        fn start(&mut self, _: serde_json::Value) -> Command {
            Command::Done {
                result: serde_json::Value::Null,
            }
        }
        fn step(&mut self, _: Event) -> Command {
            Command::Done {
                result: serde_json::Value::Null,
            }
        }
    }

    let mut b = Minimal;
    assert_eq!(b.on_token("anything"), TokenAction::Continue);
}

#[test]
fn a_block_can_stop_generation_early() {
    let mut b = Walker {
        stop_after: Some(2),
        ..Default::default()
    };
    assert_eq!(b.on_token("one"), TokenAction::Continue);
    assert_eq!(b.on_token("two"), TokenAction::Stop);
    assert_eq!(b.on_token("three"), TokenAction::Stop, "stop must latch");
}
