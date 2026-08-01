//! Where inference comes from.
//!
//! The runner talks to models only through [`InferBackend`], so the whole
//! reactor loop can be tested without a model. llama.cpp arrives behind this
//! same trait later; nothing above it should need to change.

use async_trait::async_trait;

/// What one completed generation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferResult {
    /// The generated text.
    pub text: String,
    /// Tokens consumed by the prompt.
    pub tokens_in: u32,
    /// Tokens generated.
    pub tokens_out: u32,
}

/// One inference request.
///
/// A struct rather than a growing parameter list. `images` was added after the
/// fact and would have broken every implementation had it been positional;
/// temperature, grammars, and stop sequences are all coming, and each would do
/// the same. Adding a field here with a sensible default does not.
#[derive(Debug, Default)]
pub struct InferRequest<'a> {
    /// What to generate from.
    pub prompt: &'a str,
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
    /// Images accompanying the prompt, already loaded by the host.
    ///
    /// Empty for ordinary text inference. A backend whose model has no vision
    /// capability should fail loudly rather than ignore these — silently
    /// dropping an image produces an answer about nothing, which is worse than
    /// an error because it looks like a bad model.
    pub images: &'a [Vec<u8>],
}

impl<'a> InferRequest<'a> {
    /// A text-only request.
    pub fn new(prompt: &'a str, max_tokens: u32) -> Self {
        Self {
            prompt,
            max_tokens,
            images: &[],
        }
    }
}

/// Anything that can serve an inference request.
#[async_trait]
pub trait InferBackend: Send + Sync {
    /// Generate from `req.prompt`, invoking `on_token` once per token.
    ///
    /// `on_token` returns whether to keep going; returning `false` ends
    /// generation early, which is how a guest's `Stop` verdict is honoured.
    ///
    /// The `for<'t>` is load-bearing. `#[async_trait]` rewrites elided lifetimes
    /// into named ones, which would make this closure non-generic over the
    /// token's lifetime and leave implementations unable to hand it a local
    /// `&str` — an E0597 that appears only in the implementor, with a message
    /// that does not obviously point back here.
    async fn infer(
        &self,
        req: InferRequest<'_>,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult>;

    /// Identifier recorded in the job's usage accounting.
    fn model_name(&self) -> String;

    /// Whether this backend can accept images.
    ///
    /// Defaults to false, so a backend added without thinking about vision
    /// refuses images rather than quietly discarding them.
    fn supports_images(&self) -> bool {
        false
    }
}

/// A deterministic fake that streams a fixed reply word by word.
///
/// Enough to exercise streaming, early-stop, and token accounting with no model
/// present. Registered as the `stub` provider, so a spec can select it with
/// `model = Stub "anything"` — which makes it possible to test a pipeline end to
/// end without depending on a model's wording, or on a model at all.
pub struct StubBackend {
    /// What every request generates.
    pub reply: String,
}

impl Default for StubBackend {
    fn default() -> Self {
        Self {
            reply: "a stub summary".into(),
        }
    }
}

#[async_trait]
impl InferBackend for StubBackend {
    async fn infer(
        &self,
        req: InferRequest<'_>,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        let mut out = String::new();
        let mut tokens_out = 0u32;

        // Make the images visible in the output. A caller asserting on the
        // result can then tell whether they actually arrived, instead of
        // getting a plausible answer that ignored them.
        let reply = if req.images.is_empty() {
            self.reply.clone()
        } else {
            format!("[{} image(s)] {}", req.images.len(), self.reply)
        };

        for word in reply.split_whitespace().take(req.max_tokens as usize) {
            let piece = if out.is_empty() {
                word.to_string()
            } else {
                format!(" {word}")
            };
            tokens_out += 1;

            let keep_going = on_token(&piece);
            out.push_str(&piece);
            if !keep_going {
                break;
            }

            // Yielding between tokens is not cosmetic. Without an await point
            // the entire loop runs inside a single poll, every token lands in
            // the channel at once, and the host can never interleave a guest's
            // Stop verdict — the early-stop path would look implemented while
            // being unreachable. A real backend awaits naturally; this one has
            // to do it deliberately.
            tokio::task::yield_now().await;
        }

        Ok(InferResult {
            text: out,
            tokens_in: req.prompt.split_whitespace().count() as u32,
            tokens_out,
        })
    }

    fn model_name(&self) -> String {
        "stub".into()
    }

    /// The stub accepts images so a multimodal pipeline is testable without a
    /// vision model — but it *reports* what it received rather than discarding
    /// it. Silently ignoring images is the failure this whole guard exists to
    /// prevent; a test backend that does it cannot catch anyone else doing it.
    fn supports_images(&self) -> bool {
        true
    }
}

/// Builds [`StubBackend`], registered as the `stub` provider.
pub struct StubFactory;

impl crate::backend::BackendFactory for StubFactory {
    fn provider(&self) -> &'static str {
        "stub"
    }

    fn describe(&self) -> &'static str {
        "deterministic canned responses; no model required"
    }

    /// The target becomes the reply, so a spec can choose what the stub says.
    /// An empty target keeps the default, which is what most callers want.
    fn build(&self, target: &str) -> anyhow::Result<std::sync::Arc<dyn InferBackend>> {
        Ok(std::sync::Arc::new(if target.is_empty() {
            StubBackend::default()
        } else {
            StubBackend {
                reply: target.to_string(),
            }
        }))
    }
}
