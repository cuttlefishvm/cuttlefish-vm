//! The shared interpreter every `cuttlefish-author` Script-kind block runs
//! through — one compiled wasm module, embedded in the `cuttlefish`/
//! `cuttlefishd` binaries (see `crates/cuttlefish-host/src/lib.rs`), reused
//! for every cataloged `.rhai` script rather than compiling one wasm module
//! per script.
//!
//! Job input always carries `{"__cuttlefish_script": "<script text>",
//! "input": <the real job input>}` — `run_stage` (in `cuttlefish-host`)
//! constructs this shape for a `Script`-kind node before calling `start()`;
//! a script never arrives any other way, and the real job submitter never
//! supplies source, only input.
//!
//! # Bridging a synchronous scripting language into a host-driven protocol
//!
//! Rhai has no native suspend/resume primitive (no coroutine/generator, and
//! its debugger hook is a synchronous in-stack callback, not something that
//! can hand control back to the *caller* of `Engine::eval()` and be resumed
//! later from a different top-level call).
//!
//! So this block uses a **replay-based coroutine** instead: `run()` below
//! re-executes the *entire* script from scratch on every `start`/`step`
//! call. A native `infer(prompt, max_tokens)` function is registered into
//! the engine; it consults `self.log` (answers to host commands already
//! issued, in call order) and returns the memoized answer instantly for a
//! call index already in the log. The first call index NOT yet in the log
//! is the one we need answered -- the closure stashes what command that
//! needs (`pending`) and returns `Err(..)`, which aborts `Engine::eval()`
//! immediately and propagates out as an error. `run()` checks `pending`
//! first, and only falls back to treating the eval's `Ok`/`Err` as the
//! script's real result if nothing is pending.
//!
//! This is exactly the "replay" pattern used by workflow-replay systems.
//! It costs O(n) re-execution per step (O(n^2) total for n host calls) and,
//! critically, only works if replaying the script up to the same point
//! always retraces the *same sequence* of `infer()`/etc. calls -- i.e. the
//! script's control flow before an unanswered call must be a pure function
//! of `input` and the already-memoized answers. We disabled rhai's
//! `timestamp()` builtin and gave `getrandom` a fixed fill (see below and
//! `../../.cargo/config.toml`) specifically so nothing non-replayable can
//! sneak into a script's control flow through rhai's own stdlib.
//!
//! **Known unsoundness, not fixed here**: a script that wraps any host
//! round-trip call (`infer`, `open`, `slice`, `slice_bytes`, `page_text`,
//! `page_image`) in `try { ... } catch { ... }` would intercept our
//! "suspend" error as an ordinary script-level error instead of letting it
//! propagate out of `eval()`. Rhai gives native functions no way to raise
//! an error a script genuinely cannot catch. A real implementation would
//! need either to ban `try`/`catch` around host calls (Rhai has no scope
//! for that short of a custom AST walk before running the script) or
//! accept that a catch-wrapped host call breaks the model. Flagging this
//! rather than solving it -- it's a real gap, documented in the
//! `cuttlefish-author` skill so an agent authoring a script knows not to
//! do this. (`parse_json`/`regex_test`/`regex_find`/`regex_replace_all` are
//! the exceptions: ordinary synchronous functions, not host round-trips,
//! so they're genuinely safe to wrap in `try`/`catch`.)
//!
//! **Every `Command` a Rust block can issue, a script can too**: `infer`,
//! `open`, `slice`, `slice_bytes`, `page_text`, `page_image` are all
//! registered here, sharing one suspend-or-replay decision
//! (`issue_or_replay`) parameterized only by which `Command` each call
//! builds -- capability enforcement (what a script may `open`) happens
//! host-side in the same generic `Command` dispatch loop a Rust block's
//! commands go through, so it applies here unchanged. `regex_test`/
//! `regex_find`/`regex_replace_all` round out real text extraction on top
//! of that: a script can `open`/`slice` a file and then find/pull a
//! section out of it (a heading that varies in whitespace or punctuation
//! across documents, say) without a Rust toolchain -- this interpreter
//! ships prebuilt, so nothing here costs an end user what a Rust
//! proc-block doing the same extraction would (a wasm32 target).

pub mod archive;
pub mod binary;
pub mod image_meta;

use cuttlefish_sdk::{export_block, Block, Command, Event};
use std::cell::RefCell;
use std::rc::Rc;

/// Custom `getrandom` backend for `wasm32-unknown-unknown`, wired up via
/// `--cfg getrandom_backend="custom"` in `../../.cargo/config.toml`.
///
/// Rhai's `ahash` dependency wants *a* seed, not real entropy -- it's a hash
/// DoS mitigation, not cryptography, so a fixed/cheap fill is fine here. It
/// also has to be deterministic for the replay trick above to be sound: a
/// hash seed that changed between replays of the same script could (in
/// principle, if a script iterated a map) reorder something replay depends
/// on being stable.
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    let slice = unsafe { std::slice::from_raw_parts_mut(dest, len) };
    for (i, b) in slice.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    Ok(())
}

/// Shared by `regex_test`/`regex_find`/`regex_replace_all` -- a bad pattern
/// is a script-level error each of them reports the same way.
fn compile_regex(pattern: &str) -> Result<regex::Regex, Box<rhai::EvalAltResult>> {
    regex::Regex::new(pattern).map_err(|e| format!("invalid regex `{pattern}`: {e}").into())
}

fn fail(message: String) -> Command {
    Command::Fail {
        code: "schema_validation_failed".into(),
        message,
    }
}

/// The shared suspend-or-replay decision every host-round-trip function
/// (`infer`, `open`, `slice`, `slice_bytes`, `page_text`, `page_image`)
/// makes, factored out so each one only has to build its own `Command` --
/// see the module doc's "Bridging a synchronous scripting language..."
/// section for why this needs to exist at all.
///
/// Divergence detection is generic across every call KIND, not just
/// `infer`: a script that opened a file on one run and called `infer`
/// instead at the same call index on a replay is exactly as
/// nondeterministic as a script that changed its prompt.
fn issue_or_replay(
    command: Command,
    call_index: &Rc<RefCell<usize>>,
    pending: &Rc<RefCell<Option<Command>>>,
    log: &[(Command, serde_json::Value)],
) -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
    let idx = *call_index.borrow();

    if let Some((recorded_command, answer)) = log.get(idx) {
        if recorded_command != &command {
            *pending.borrow_mut() = Some(Command::Fail {
                code: "nondeterministic_replay".into(),
                message: format!(
                    "replay diverged at host call {idx}: originally {recorded_command:?}, now \
                     {command:?} -- a script's control flow before a host call must depend only \
                     on `input` and previously-answered host calls"
                ),
            });
            return Err("__cf_suspend_for_host_command".into());
        }
        *call_index.borrow_mut() += 1;
        return rhai::serde::to_dynamic(answer)
            .map_err(|e| format!("replaying a host call's answer: {e}").into());
    }

    *pending.borrow_mut() = Some(command);
    // Aborts Engine::eval() right here. See the module doc's caveat: a
    // script-level try/catch around this call would intercept it instead
    // of letting it propagate.
    Err("__cf_suspend_for_host_command".into())
}

#[derive(Default)]
struct RhaiBlock {
    script: String,
    /// The `input` variable scripts see -- the real job's input, distinct
    /// from the wrapper object (`{__cuttlefish_script, input}`) `start`
    /// actually receives.
    input: serde_json::Value,
    /// Host commands already issued and answered, in call order: the exact
    /// `Command` that was sent, paired with the answer that came back.
    /// Replayed into the script on every re-run; see the module doc. The
    /// recorded `Command` (not just the answer) is what makes divergence
    /// detection possible -- see `run()`'s `infer` closure.
    log: Vec<(Command, serde_json::Value)>,
    /// The `Command` `run()` last suspended on, so the next `step()` call
    /// knows what to pair the answering `Event` with in `log`. `None`
    /// whenever no command is in flight (before the first call, or right
    /// after the script finished).
    pending_command: Option<Command>,
}

impl RhaiBlock {
    /// (Re-)run the whole script from the top. Returns the next `Command`:
    /// `Infer` if the script (this time) ran into an `infer()` call with no
    /// memoized answer yet, `Done`/`Fail` if it ran to completion.
    ///
    /// Callers (`start`/`step`) are responsible for stashing the return
    /// value into `self.pending_command` when it's an `Infer` -- `run`
    /// itself takes `&self`, not `&mut self`, so it can't do that stashing
    /// on its own.
    fn run(&self) -> Command {
        let mut engine = rhai::Engine::new();
        let mut scope = rhai::Scope::new();

        let dynamic_input = match rhai::serde::to_dynamic(&self.input) {
            Ok(d) => d,
            Err(e) => return fail(format!("converting input to rhai value: {e}")),
        };
        scope.push("input", dynamic_input);

        // Ordinary synchronous function, not a host round-trip -- doesn't
        // touch call_index/log, so it has no replay implications and is
        // safe for a script to wrap in try/catch (unlike infer()). Exists
        // so a script asking the model for structured output has some way
        // to turn the reply into a real record instead of only raw string
        // ops; a script that calls this on an unparseable reply gets a
        // normal Rhai runtime error and the job fails, matching the
        // "throw on an unreadable answer, never default" rule.
        // --- Binary inspection ---------------------------------------
        //
        // All pure, like parse_json: they take the base64 a script already
        // holds from slice_bytes and compute. No host round trip means no
        // call_index, no replay entry, and safe use inside try/catch.
        //
        // Base64 in and (where bytes come back) base64 out, so results feed
        // straight into each other -- gunzip a .tar.gz, hand the result to
        // tar_entries -- without inventing a byte type the replay log has no
        // way to serialize.
        {
            use crate::{archive, binary, image_meta};

            fn bytes_of(b64: &str, who: &str) -> Result<Vec<u8>, Box<rhai::EvalAltResult>> {
                binary::decode(b64).map_err(|e| format!("{who}: {e}").into())
            }
            fn to_dyn(
                value: serde_json::Value,
                who: &str,
            ) -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                rhai::serde::to_dynamic(&value)
                    .map_err(|e| format!("{who}: converting to a script value: {e}").into())
            }

            engine.register_fn(
                "identify",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    to_dyn(binary::identify(&bytes_of(b64, "identify")?), "identify")
                },
            );
            engine.register_fn(
                "entropy",
                |b64: &str| -> Result<f64, Box<rhai::EvalAltResult>> {
                    Ok(binary::entropy(&bytes_of(b64, "entropy")?))
                },
            );
            engine.register_fn(
                "byte_histogram",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let counts = binary::byte_histogram(&bytes_of(b64, "byte_histogram")?);
                    to_dyn(serde_json::json!(counts), "byte_histogram")
                },
            );
            engine.register_fn(
                "strings",
                |b64: &str, min_len: i64| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let found =
                        binary::strings(&bytes_of(b64, "strings")?, min_len.max(1) as usize);
                    to_dyn(serde_json::json!(found), "strings")
                },
            );
            engine.register_fn(
                "hexdump",
                |b64: &str, base_offset: i64| -> Result<String, Box<rhai::EvalAltResult>> {
                    Ok(binary::hexdump(
                        &bytes_of(b64, "hexdump")?,
                        base_offset.max(0) as u64,
                    ))
                },
            );
            engine.register_fn(
                "dimensions",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let d = image_meta::dimensions(&bytes_of(b64, "dimensions")?)
                        .map_err(|e| format!("dimensions: {e}"))?;
                    to_dyn(d, "dimensions")
                },
            );
            engine.register_fn(
                "exif",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let e = image_meta::exif(&bytes_of(b64, "exif")?)
                        .map_err(|e| format!("exif: {e}"))?;
                    to_dyn(e, "exif")
                },
            );
            engine.register_fn(
                "png_chunks",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let c = image_meta::png_chunks(&bytes_of(b64, "png_chunks")?)
                        .map_err(|e| format!("png_chunks: {e}"))?;
                    to_dyn(serde_json::json!(c), "png_chunks")
                },
            );
            engine.register_fn(
                "jpeg_segments",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let seg = image_meta::jpeg_segments(&bytes_of(b64, "jpeg_segments")?)
                        .map_err(|e| format!("jpeg_segments: {e}"))?;
                    to_dyn(serde_json::json!(seg), "jpeg_segments")
                },
            );
            engine.register_fn(
                "tar_entries",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let entries = archive::tar_entries(&bytes_of(b64, "tar_entries")?)
                        .map_err(|e| format!("tar_entries: {e}"))?;
                    to_dyn(serde_json::json!(entries), "tar_entries")
                },
            );
            engine.register_fn(
                "zip_entries",
                |b64: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let entries = archive::zip_entries(&bytes_of(b64, "zip_entries")?)
                        .map_err(|e| format!("zip_entries: {e}"))?;
                    to_dyn(serde_json::json!(entries), "zip_entries")
                },
            );
            // max_bytes is required, not defaulted: a caller has to decide
            // what it is willing to hold in memory, because running out is a
            // wasm trap that kills the whole fan-out item rather than
            // failing this one archive.
            engine.register_fn(
                "gunzip",
                |b64: &str, max_bytes: i64| -> Result<String, Box<rhai::EvalAltResult>> {
                    let out = archive::gunzip(&bytes_of(b64, "gunzip")?, max_bytes.max(0) as usize)
                        .map_err(|e| format!("gunzip: {e}"))?;
                    Ok(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        out,
                    ))
                },
            );
            engine.register_fn(
                "inflate",
                |b64: &str, max_bytes: i64| -> Result<String, Box<rhai::EvalAltResult>> {
                    let out =
                        archive::inflate(&bytes_of(b64, "inflate")?, max_bytes.max(0) as usize)
                            .map_err(|e| format!("inflate: {e}"))?;
                    Ok(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        out,
                    ))
                },
            );
        }

        engine.register_fn(
            "parse_json",
            |text: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                let value: serde_json::Value =
                    serde_json::from_str(text).map_err(|e| format!("parse_json: {e}"))?;
                rhai::serde::to_dynamic(&value)
                    .map_err(|e| format!("parse_json: converting to a script value: {e}").into())
            },
        );

        // Also ordinary synchronous functions -- exist so a script that
        // read a real file via open()/slice() has some way to find/extract
        // structure in it (a section heading that varies in whitespace or
        // punctuation across documents, say) beyond plain substring
        // search. An invalid pattern is a normal script error (unlike a
        // real "no match" outcome, which regex_find reports via its
        // `found` field, not an error -- not matching is an expected,
        // common result, not a failure).
        engine.register_fn(
            "regex_test",
            |text: &str, pattern: &str| -> Result<bool, Box<rhai::EvalAltResult>> {
                let re = compile_regex(pattern)?;
                Ok(re.is_match(text))
            },
        );
        engine.register_fn(
            "regex_find",
            |text: &str, pattern: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                let re = compile_regex(pattern)?;
                let found = match re.find(text) {
                    Some(m) => serde_json::json!({
                        "found": true,
                        "start": m.start(),
                        "end": m.end(),
                        "text": m.as_str(),
                    }),
                    None => serde_json::json!({
                        "found": false,
                        "start": -1,
                        "end": -1,
                        "text": "",
                    }),
                };
                rhai::serde::to_dynamic(&found)
                    .map_err(|e| format!("regex_find: converting to a script value: {e}").into())
            },
        );
        engine.register_fn(
            "regex_replace_all",
            |text: &str,
             pattern: &str,
             replacement: &str|
             -> Result<String, Box<rhai::EvalAltResult>> {
                let re = compile_regex(pattern)?;
                Ok(re.replace_all(text, replacement).into_owned())
            },
        );

        let call_index = Rc::new(RefCell::new(0usize));
        let pending: Rc<RefCell<Option<Command>>> = Rc::new(RefCell::new(None));
        let log = Rc::new(self.log.clone());

        // Every host-round-trip function below (as opposed to a pure one
        // like parse_json) shares this shape: build the Command the call
        // represents, then hand it to issue_or_replay, which either
        // returns the memoized answer from a prior run or suspends. Each
        // closure needs its own clone of call_index/pending/log (all cheap
        // -- an Rc bump or a couple of words), since rhai::Engine::register_fn
        // takes ownership of what it's given and each arity is a distinct
        // closure type.
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "infer",
                move |prompt: &str,
                      max_tokens: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::Infer {
                            prompt: prompt.to_string(),
                            max_tokens: max_tokens.max(0) as u32,
                            images: Vec::new(),
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "open",
                move |path: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::Open {
                            path: path.to_string(),
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "slice",
                move |handle: i64,
                      offset: i64,
                      len: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::Slice {
                            handle: handle.max(0) as u32,
                            offset: offset.max(0) as u64,
                            len: len.max(0) as u64,
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "slice_bytes",
                move |handle: i64,
                      offset: i64,
                      len: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::SliceBytes {
                            handle: handle.max(0) as u32,
                            offset: offset.max(0) as u64,
                            len: len.max(0) as u64,
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "page_text",
                move |handle: i64, page: i64| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::PageText {
                            handle: handle.max(0) as u32,
                            page: page.max(0) as u32,
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "page_image",
                move |handle: i64, page: i64| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::PageImage {
                            handle: handle.max(0) as u32,
                            page: page.max(0) as u32,
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        // Image transforms are host calls, unlike the metadata builtins
        // above: they need a decoder, so they suspend and replay like
        // infer/open rather than computing in the guest. Both yield a new
        // image handle, usable anywhere a file-backed image is -- including
        // as an infer() image.
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "image_resize",
                move |handle: i64,
                      max_width: i64,
                      max_height: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::ImageOp {
                            handle: handle.max(0) as u32,
                            op: cuttlefish_sdk::ImageOperation::Resize {
                                max_width: max_width.max(0) as u32,
                                max_height: max_height.max(0) as u32,
                            },
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }
        {
            let (call_index, pending, log) = (call_index.clone(), pending.clone(), log.clone());
            engine.register_fn(
                "image_crop",
                move |handle: i64,
                      x: i64,
                      y: i64,
                      width: i64,
                      height: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    issue_or_replay(
                        Command::ImageOp {
                            handle: handle.max(0) as u32,
                            op: cuttlefish_sdk::ImageOperation::Crop {
                                x: x.max(0) as u32,
                                y: y.max(0) as u32,
                                width: width.max(0) as u32,
                                height: height.max(0) as u32,
                            },
                        },
                        &call_index,
                        &pending,
                        &log,
                    )
                },
            );
        }

        let result = engine.eval_with_scope::<rhai::Dynamic>(&mut scope, &self.script);

        if let Some(cmd) = pending.borrow_mut().take() {
            return cmd;
        }

        // The script finished (Done or a real script error) without
        // suspending on a new infer() call -- but if it also finished
        // *without consuming every already-answered log entry*, that's
        // still a divergence the per-call comparison above can't see: this
        // run's control flow needed fewer infer() answers than a prior run
        // did to reach the same point, which is only possible if something
        // other than `input`/the log influenced it. Silently returning
        // whatever this run computed would be exactly the silent-wrong-
        // result failure mode this whole mechanism exists to prevent.
        let consumed = *call_index.borrow();
        if consumed < self.log.len() {
            return Command::Fail {
                code: "nondeterministic_replay".into(),
                message: format!(
                    "replay diverged: this run finished after {consumed} infer() call(s), but \
                     {} were already answered from a prior run -- a script's control flow must \
                     depend only on `input` and previously-answered infer() calls",
                    self.log.len()
                ),
            };
        }

        match result {
            Ok(value) => match rhai::serde::from_dynamic::<serde_json::Value>(&value) {
                Ok(json) => Command::Done { result: json },
                Err(e) => fail(format!("converting rhai result to json: {e}")),
            },
            // A failure here is the script's own logic going wrong at run
            // time (a Rhai `EvalAltResult` -- missing function, index out of
            // bounds, an explicit `throw`, and so on). `catalog add` already
            // rejects a script that fails to *parse* (see
            // `catalog::read_script_signature`), so anything reaching this
            // arm is a run-time failure, not a shape mismatch -- it must not
            // reuse `schema_validation_failed`, which callers rely on
            // meaning "input/output didn't match the declared signature".
            Err(e) => Command::Fail {
                code: "script_error".into(),
                message: format!("script error: {e}"),
            },
        }
    }

    /// Run the script and, if it suspended on a new host-call command (not
    /// a `nondeterministic_replay` `Fail`, which also flows through `run`'s
    /// `pending` mechanism but must never be stashed as if it were a real
    /// in-flight command), remember it so the next `step()` can pair the
    /// answering `Event` with it in `log`.
    fn run_and_track_pending(&mut self) -> Command {
        let cmd = self.run();
        // Deliberately inverted: list the commands the host does *not*
        // answer, and treat everything else as a host call. The allowlist
        // this replaced was a `matches!`, so the compiler could not warn
        // when a newly added command fell off it — and one that falls off
        // is never stashed as pending, so the host's reply arrives to find
        // nothing in flight and the guest panics. That is a wasm trap, i.e.
        // the whole item, for a one-line omission. Getting it wrong this
        // way round merely stashes a command that will never be answered,
        // which is harmless because Done/Fail end the block anyway.
        let is_host_call = !matches!(
            cmd,
            Command::Emit { .. } | Command::Done { .. } | Command::Fail { .. }
        );
        self.pending_command = is_host_call.then(|| cmd.clone());
        cmd
    }
}

impl Block for RhaiBlock {
    fn start(&mut self, input: serde_json::Value) -> Command {
        self.script = match input.get("__cuttlefish_script").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return fail(
                    "job input must carry a string `__cuttlefish_script` field (populated by \
                     run_stage for a Script-kind node -- this block should never be reached \
                     with anything else)"
                        .into(),
                )
            }
        };
        self.input = input
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        self.log.clear();
        self.pending_command = None;
        self.run_and_track_pending()
    }

    fn step(&mut self, event: Event) -> Command {
        // Every arm below does the same thing: take the one command this
        // block itself issued (there is always exactly one in flight --
        // the host only ever answers what was asked), turn the event's
        // payload into the JSON value the script sees, log the pair, and
        // resume. `pending_command` came from `run_and_track_pending`,
        // which stashes it for exactly this purpose.
        let command = self.pending_command.take().unwrap_or_else(|| {
            panic!(
                "step() received {event:?} but no command was pending -- the host only sends an \
                 event in response to the command this block itself issued"
            )
        });

        let answer = match event {
            Event::InferDone { text, .. } => serde_json::Value::String(text),
            Event::Opened { handle, len, kind } => match serde_json::to_value(kind) {
                Ok(kind) => serde_json::json!({ "handle": handle, "len": len, "kind": kind }),
                Err(e) => return fail(format!("converting an Opened event: {e}")),
            },
            Event::Sliced { text, next_offset } => {
                serde_json::json!({ "text": text, "next_offset": next_offset })
            }
            Event::SlicedBytes {
                bytes_base64,
                next_offset,
            } => {
                serde_json::json!({ "bytes_base64": bytes_base64, "next_offset": next_offset })
            }
            Event::PageTexted { text } => serde_json::json!({ "text": text }),
            Event::PageImaged { handle, len } => {
                serde_json::json!({ "handle": handle, "len": len })
            }
            other => {
                return Command::Fail {
                    code: "unexpected_event".into(),
                    message: format!(
                        "this interpreter has no script-facing call that produces {other:?}"
                    ),
                }
            }
        };

        self.log.push((command, answer));
        self.run_and_track_pending()
    }
}

export_block!(RhaiBlock);

/// A `Block` is unit-testable natively, no wasm involved (see the
/// `cuttlefish-sdk` crate doc) -- these specifically exercise the
/// `nondeterministic_replay` divergence check, which end-to-end tests in
/// `crates/cuttlefish-host/tests/rhai_interpreter.rs` can't easily force
/// (a real script's replay genuinely never diverges there; here `log` is
/// hand-falsified to simulate a divergence directly).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replay_that_diverges_from_the_recorded_command_fails_loudly() {
        let block = RhaiBlock {
            script: "infer(\"real prompt\", 16)".to_string(),
            input: serde_json::Value::Null,
            log: vec![(
                Command::Infer {
                    prompt: "a different prompt than the script actually sends".to_string(),
                    max_tokens: 99,
                    images: Vec::new(),
                },
                serde_json::json!("some prior answer"),
            )],
            pending_command: None,
        };

        match block.run() {
            Command::Fail { code, message } => {
                assert_eq!(code, "nondeterministic_replay");
                assert!(message.contains("diverged"), "{message}");
            }
            other => panic!("expected a nondeterministic_replay Fail, got {other:?}"),
        }
    }

    #[test]
    fn a_replay_that_matches_the_recorded_command_succeeds() {
        let block = RhaiBlock {
            script: "infer(\"real prompt\", 16)".to_string(),
            input: serde_json::Value::Null,
            log: vec![(
                Command::Infer {
                    prompt: "real prompt".to_string(),
                    max_tokens: 16,
                    images: Vec::new(),
                },
                serde_json::json!("the memoized answer"),
            )],
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!("the memoized answer")
            }
        );
    }

    #[test]
    fn a_script_runtime_error_fails_with_script_error_not_schema_validation_failed() {
        // `x()` parses fine (catalog add's compile check would let it
        // through) but there is no such function -- a genuine runtime
        // failure inside the script, distinct from the input/output shape
        // mismatches `schema_validation_failed` means elsewhere.
        let block = RhaiBlock {
            script: "x()".to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        match block.run() {
            Command::Fail { code, message } => {
                assert_eq!(code, "script_error");
                assert!(message.contains("script error"), "{message}");
            }
            other => panic!("expected a script_error Fail, got {other:?}"),
        }
    }

    #[test]
    fn a_replay_that_finishes_without_consuming_every_logged_answer_fails_loudly() {
        // A script whose control flow, on this run, finishes (evaluates to
        // a plain literal) without ever calling infer() at all -- despite
        // the log already holding one answered call from a prior run of
        // (nominally) the same script. Finishing "early" like this is just
        // as much a divergence as a per-call mismatch: this run needed
        // fewer answers to reach a result than a prior run of the same
        // script/input did.
        let block = RhaiBlock {
            script: "42".to_string(),
            input: serde_json::Value::Null,
            log: vec![(
                Command::Infer {
                    prompt: "a prompt from a prior, apparently different run".to_string(),
                    max_tokens: 16,
                    images: Vec::new(),
                },
                serde_json::json!("some prior answer"),
            )],
            pending_command: None,
        };

        match block.run() {
            Command::Fail { code, message } => {
                assert_eq!(code, "nondeterministic_replay");
                assert!(message.contains("diverged"), "{message}");
            }
            other => panic!("expected a nondeterministic_replay Fail, got {other:?}"),
        }
    }

    #[test]
    fn parse_json_turns_a_json_string_into_a_real_record() {
        let block = RhaiBlock {
            script: r#"let v = parse_json("{\"score\": 7, \"verdict\": \"pass\"}"); #{ verdict: v.verdict, doubled: v.score * 2 }"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!({"verdict": "pass", "doubled": 14})
            }
        );
    }

    #[test]
    fn parse_json_on_unparseable_text_fails_the_job_not_a_silent_default() {
        let block = RhaiBlock {
            script: r#"parse_json("not json")"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        match block.run() {
            Command::Fail { message, .. } => {
                assert!(message.contains("parse_json"), "{message}");
            }
            other => panic!("expected a Fail naming parse_json, got {other:?}"),
        }
    }

    #[test]
    fn parse_json_is_safe_to_wrap_in_try_catch_unlike_infer() {
        // Unlike infer(), parse_json isn't a suspend point -- a script may
        // catch its error without corrupting the replay mechanism. (try/
        // catch is a statement in Rhai, not an expression, hence the `let`
        // + trailing-expression form rather than using its value directly.)
        let block = RhaiBlock {
            script: r#"
                let v = #{};
                try { v = parse_json("not json"); } catch { v = #{ fallback: true }; }
                v
            "#
            .to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!({"fallback": true})
            }
        );
    }

    #[test]
    fn regex_test_matches_and_does_not_match() {
        let block = RhaiBlock {
            script:
                r#"#{ a: regex_test("hello world", "wor.d"), b: regex_test("hello world", "xyz") }"#
                    .to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!({"a": true, "b": false})
            }
        );
    }

    #[test]
    fn regex_find_reports_position_and_text_of_the_first_match() {
        let block = RhaiBlock {
            script: r#"regex_find("see item 42 and item 7", "item \\d+")"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!({
                    "found": true,
                    "start": 4,
                    "end": 11,
                    "text": "item 42",
                })
            }
        );
    }

    #[test]
    fn regex_find_reports_not_found_rather_than_erroring() {
        let block = RhaiBlock {
            script: r#"regex_find("no numbers here", "\\d+").found"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!(false)
            }
        );
    }

    #[test]
    fn regex_replace_all_replaces_every_match() {
        let block = RhaiBlock {
            script: r#"regex_replace_all("a1 b2 c3", "[a-z](\\d)", "n$1")"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!("n1 n2 n3")
            }
        );
    }

    #[test]
    fn an_invalid_pattern_fails_the_job_not_a_silent_default() {
        let block = RhaiBlock {
            script: r#"regex_test("x", "(unclosed")"#.to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        match block.run() {
            Command::Fail { message, .. } => {
                assert!(message.contains("invalid regex"), "{message}");
            }
            other => panic!("expected a Fail naming the invalid regex, got {other:?}"),
        }
    }

    #[test]
    fn a_whitespace_tolerant_heading_match_works_like_the_real_10k_extraction_case() {
        // The exact pattern that motivated this: SEC filing HTML sometimes
        // renders a heading with its letters spaced out ("RI SK FACTORS")
        // -- a plain substring search misses it, but a regex with `\s*`
        // between letters catches it.
        let block = RhaiBlock {
            script: r#"
                let doc = "Item 1A. RI SK FACTORS\nOur business faces risks.";
                regex_find(doc, "(?i)item\\s*1a\\.?\\s*ri\\s*sk\\s*factors").found
            "#
            .to_string(),
            input: serde_json::Value::Null,
            log: Vec::new(),
            pending_command: None,
        };

        assert_eq!(
            block.run(),
            Command::Done {
                result: serde_json::json!(true)
            }
        );
    }
}
