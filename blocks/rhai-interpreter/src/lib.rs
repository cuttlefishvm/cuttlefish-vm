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
//! **Known unsoundness, not fixed here**: a script that wraps the `infer()`
//! call in `try { ... } catch { ... }` would intercept our "suspend" error
//! as an ordinary script-level error instead of letting it propagate out of
//! `eval()`. Rhai gives native functions no way to raise an error a script
//! genuinely cannot catch. A real implementation would need either to ban
//! `try`/`catch` around host calls (Rhai has no scope for that short of a
//! custom AST walk before running the script) or accept that a
//! catch-wrapped host call breaks the model. Flagging this rather than
//! solving it -- it's a real gap, documented in the `cuttlefish-author`
//! skill so an agent authoring a script knows not to do this.
//!
//! **Only `infer` is wired in this initial cut.** `open`/`slice`/
//! `page_text`/`page_image` are architecturally identical to add (the same
//! log-and-suspend mechanism), but wiring each one is left as a documented
//! follow-up rather than shipped speculatively here — see the
//! `cuttlefish-author` skill for what a script can and can't do today.

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

fn fail(message: String) -> Command {
    Command::Fail {
        code: "schema_validation_failed".into(),
        message,
    }
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

        let call_index = Rc::new(RefCell::new(0usize));
        let pending: Rc<RefCell<Option<Command>>> = Rc::new(RefCell::new(None));
        let log = self.log.clone();

        {
            let call_index = call_index.clone();
            let pending = pending.clone();
            engine.register_fn(
                "infer",
                move |prompt: &str,
                      max_tokens: i64|
                      -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
                    let idx = *call_index.borrow();
                    let requested = Command::Infer {
                        prompt: prompt.to_string(),
                        max_tokens: max_tokens.max(0) as u32,
                        images: Vec::new(),
                    };

                    if let Some((recorded_command, answer)) = log.get(idx) {
                        // Replay only produces a correct result if the
                        // sequence of infer() calls is identical to the
                        // original run -- same prompt, same max_tokens, at
                        // the same call index. If it isn't, the script's
                        // control flow depended on something other than
                        // `input` and the prior answers (see the module
                        // doc), and continuing would silently return a
                        // wrong result rather than failing loudly.
                        if recorded_command != &requested {
                            *pending.borrow_mut() = Some(Command::Fail {
                                code: "nondeterministic_replay".into(),
                                message: format!(
                                    "replay diverged at infer() call {idx}: originally \
                                     {recorded_command:?}, now {requested:?} -- a script's \
                                     control flow before an infer() call must depend only on \
                                     `input` and previously-answered infer() calls"
                                ),
                            });
                            return Err("__cf_suspend_for_host_command".into());
                        }
                        *call_index.borrow_mut() += 1;
                        return rhai::serde::to_dynamic(answer)
                            .map_err(|e| format!("replaying infer() answer: {e}").into());
                    }

                    *pending.borrow_mut() = Some(requested);
                    // Aborts Engine::eval() right here. See the module doc's
                    // caveat: a script-level try/catch around this call
                    // would intercept it instead of letting it propagate.
                    Err("__cf_suspend_for_host_command".into())
                },
            );
        }

        let result = engine.eval_with_scope::<rhai::Dynamic>(&mut scope, &self.script);

        if let Some(cmd) = pending.borrow_mut().take() {
            return cmd;
        }

        match result {
            Ok(value) => match rhai::serde::from_dynamic::<serde_json::Value>(&value) {
                Ok(json) => Command::Done { result: json },
                Err(e) => fail(format!("converting rhai result to json: {e}")),
            },
            Err(e) => fail(format!("script error: {e}")),
        }
    }

    /// Run the script and, if it suspended on a new `Infer` command (not a
    /// `nondeterministic_replay` `Fail`, which also flows through `run`'s
    /// `pending` mechanism but must never be stashed as if it were a real
    /// in-flight command), remember it so the next `step()` can pair the
    /// answering `Event` with it in `log`.
    fn run_and_track_pending(&mut self) -> Command {
        let cmd = self.run();
        self.pending_command = matches!(cmd, Command::Infer { .. }).then(|| cmd.clone());
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
        match event {
            // `infer()` in script sees whatever text the model produced.
            Event::InferDone { text, .. } => {
                let command = self.pending_command.take().expect(
                    "step() received InferDone but no Infer command was pending -- the host \
                     only sends an Event in response to the Command this block itself issued",
                );
                self.log.push((command, serde_json::Value::String(text)));
                self.run_and_track_pending()
            }
            other => Command::Fail {
                code: "unexpected_event".into(),
                message: format!(
                    "this interpreter only wires up Infer round-trips in this initial cut: {other:?}"
                ),
            },
        }
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
}
