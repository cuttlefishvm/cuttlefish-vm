//! Acceptance checks: what "done" means for a node beyond its declared type.
//!
//! A node's `Ty` signature says what *shape* its output has. That is a real
//! contract, and it catches real mistakes — but two failure modes slip
//! straight through it. A truncated model reply still parses as `json` and
//! satisfies a `json` signature completely. And output that is well-formed,
//! correctly typed, and simply *wrong* is invisible to any type system.
//!
//! `accept = [ ... ]` closes that gap with an ordered, short-circuiting list
//! of checks. Order is load-bearing rather than cosmetic: [`AcceptCheck::Schema`]
//! is deterministic and costs nothing, while [`AcceptCheck::Judge`] costs a
//! whole inference — so a schema-first list never pays for a judge on output
//! that is structurally broken, which is also the output a judge grades least
//! coherently.
//!
//! This module only reaches verdicts. Reacting to a failed one — retrying,
//! rerouting, escalating — is the ladder's job, in [`crate::runner`].

use crate::infer::{InferBackend, InferRequest};
use cuttlefish_core::graph::AcceptCheck;
use cuttlefish_core::spec::ModelRef;
use std::sync::Arc;

/// Upper bound on a judge's reply.
///
/// A verdict is `{"accept": bool, "reason": "..."}` — small by construction.
/// Capping it keeps a judge that starts rambling from costing more than the
/// work it is grading, and a truncated ramble lands as
/// [`JudgeVerdict::Unusable`] rather than being mistaken for a verdict.
const JUDGE_MAX_TOKENS: u32 = 256;

/// What a judge concluded about one output.
#[derive(Debug)]
pub enum JudgeVerdict {
    /// `{"accept": true, ...}` — or there were no judges to ask.
    Accepted,
    /// `{"accept": false, "reason": "..."}`.
    ///
    /// The reason is retained because it becomes the text a human reads in
    /// `cuttlefish escalations`, long after the run.
    Rejected(String),
    /// The judge's reply did not parse as a verdict, or its inference
    /// errored.
    ///
    /// Deliberately *not* folded into [`JudgeVerdict::Rejected`]. "The judge
    /// never returned a usable verdict" and "the judge read this and said no"
    /// call for completely different responses from whoever reads the
    /// escalation — one is a broken grader, the other is broken work — and
    /// collapsing them sends that person hunting for a rejection that never
    /// happened.
    Unusable(String),
}

/// A node's `accept` list, with schemas already compiled.
///
/// Compiled once at daemon startup rather than per attempt: a malformed
/// schema is a property of the spec, so it should stop the daemon coming up
/// rather than surface as a bizarre acceptance failure partway through a
/// campaign.
pub struct CompiledChecks {
    schemas: Vec<(std::path::PathBuf, jsonschema::Validator)>,
    judges: Vec<(Option<ModelRef>, String)>,
}

impl CompiledChecks {
    /// Read and compile every `Schema`, and collect every `Judge`.
    ///
    /// Fails if a schema file is unreadable or is not a valid JSON Schema.
    pub fn compile(checks: &[AcceptCheck]) -> anyhow::Result<Self> {
        let mut schemas = Vec::new();
        let mut judges = Vec::new();
        for check in checks {
            match check {
                AcceptCheck::Schema(path) => {
                    let text = std::fs::read_to_string(path).map_err(|e| {
                        anyhow::anyhow!("reading accept schema {}: {e}", path.display())
                    })?;
                    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                        anyhow::anyhow!("accept schema {} is not valid JSON: {e}", path.display())
                    })?;
                    let validator = jsonschema::validator_for(&value).map_err(|e| {
                        anyhow::anyhow!(
                            "accept schema {} is not a valid JSON Schema: {e}",
                            path.display()
                        )
                    })?;
                    schemas.push((path.clone(), validator));
                }
                AcceptCheck::Judge { model, prompt } => {
                    judges.push((model.clone(), prompt.clone()))
                }
            }
        }
        Ok(Self { schemas, judges })
    }

    /// Whether this node declares any check at all.
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.judges.is_empty()
    }

    /// Validate `value` against every compiled schema.
    ///
    /// Reports *all* violations rather than only the first, matching
    /// `cuttlefish validate-json`: someone fixing a prompt wants the whole
    /// list, not one round trip per mistake.
    pub fn check_schemas(&self, value: &serde_json::Value) -> Result<(), String> {
        for (path, validator) in &self.schemas {
            let violations: Vec<String> = validator
                .iter_errors(value)
                .map(|e| format!("{}: {e}", e.instance_path()))
                .collect();
            if !violations.is_empty() {
                return Err(format!(
                    "does not conform to {}:\n{}",
                    path.display(),
                    violations.join("\n")
                ));
            }
        }
        Ok(())
    }

    /// Ask every judge, in order, stopping at the first non-acceptance.
    ///
    /// `default` serves a judge that named no model of its own; `alternates`
    /// serves one that did. A judge naming a model absent from `alternates`
    /// is [`JudgeVerdict::Unusable`] rather than a panic — startup resolution
    /// should have caught it, so reaching here means a bug, and failing the
    /// attempt beats taking the daemon down mid-campaign.
    pub async fn run_judges(
        &self,
        input: &serde_json::Value,
        output: &serde_json::Value,
        default: &Arc<dyn InferBackend>,
        alternates: &crate::runner::Alternates,
    ) -> JudgeVerdict {
        for (model, prompt) in &self.judges {
            let backend = match model {
                None => default,
                Some(m) => match alternates.get(m) {
                    Some(b) => b,
                    None => {
                        return JudgeVerdict::Unusable(format!(
                            "judge names model `{m}`, which was not resolved at startup"
                        ))
                    }
                },
            };

            let full = judge_prompt(prompt, input, output);
            let mut sink = |_: &str| true;
            let reply = match backend
                .infer(
                    InferRequest {
                        prompt: &full,
                        max_tokens: JUDGE_MAX_TOKENS,
                        images: &[],
                    },
                    &mut sink,
                )
                .await
            {
                Ok(r) => r.text,
                Err(e) => return JudgeVerdict::Unusable(format!("judge inference failed: {e}")),
            };

            match parse_verdict(&reply) {
                Ok(true) => continue,
                Ok(false) => {
                    return JudgeVerdict::Rejected(
                        verdict_reason(&reply).unwrap_or_else(|| "no reason given".to_string()),
                    )
                }
                Err(why) => return JudgeVerdict::Unusable(why),
            }
        }
        JudgeVerdict::Accepted
    }
}

impl std::fmt::Debug for CompiledChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `jsonschema::Validator` is not Debug; name what's here instead of
        // trying to render it.
        f.debug_struct("CompiledChecks")
            .field("schemas", &self.schemas.len())
            .field("judges", &self.judges.len())
            .finish()
    }
}

/// The author's prompt, then the input and the output under delimited
/// headings.
///
/// Both are needed: "does this finding cite specific numbers *from the
/// input*" — the motivating case — is unanswerable with the output alone.
fn judge_prompt(prompt: &str, input: &serde_json::Value, output: &serde_json::Value) -> String {
    format!(
        "{prompt}\n\n\
         --- INPUT ---\n{input}\n\n\
         --- OUTPUT UNDER REVIEW ---\n{output}\n\n\
         --- END ---\n\
         Reply with JSON only: {{\"accept\": true|false, \"reason\": \"...\"}}"
    )
}

/// Pull `accept` out of a judge's reply.
///
/// Tolerates surrounding prose, since a small model asked for JSON often
/// wraps it — but a reply with no object at all, or an object without a
/// boolean `accept`, is unusable rather than a rejection.
fn parse_verdict(reply: &str) -> Result<bool, String> {
    let value = extract_json(reply)
        .ok_or_else(|| format!("judge reply contained no JSON object: {reply:?}"))?;
    value
        .get("accept")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("judge reply has no boolean `accept` field: {reply:?}"))
}

fn verdict_reason(reply: &str) -> Option<String> {
    extract_json(reply)?
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The first `{...}` span in `reply` that parses as JSON.
fn extract_json(reply: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(reply.trim()) {
        return Some(v);
    }
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&reply[start..=end]).ok()
}
