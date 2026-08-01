//! Inference from an embedded llama.cpp, with no separate server process.
//!
//! Enabled by the `llamacpp` cargo feature, which is **off by default** and
//! deliberately so: `llama-cpp-sys-2` compiles llama.cpp from source, which
//! needs cmake, a C++ toolchain, and libclang for bindgen. Most users are better
//! served by [`crate::ollama`], which needs none of that. Making this opt-in
//! keeps a heavyweight native build off the path of everyone who does not want
//! it — and the registry means nothing else changes when it is absent.
//!
//! # What embedding buys over talking to a server
//!
//! - No second process to install, run, or keep alive.
//! - Direct control of the context, so a job's KV cache is genuinely its own
//!   rather than something a shared server decides how to reuse.
//! - The model is loaded once into this process; a job does not pay to reach
//!   across a socket for every token.
//!
//! The cost is the build, and that it pins this project to llama.cpp's own
//! release cadence and API.
//!
//! # Threading
//!
//! llama.cpp is synchronous and CPU-bound, while [`InferBackend::infer`] is
//! async. Running generation inline would stall the daemon's runtime for the
//! whole of a job's inference — every other job's I/O included.
//!
//! So generation runs on a blocking thread and tokens come back over a channel,
//! with a shared flag carrying the guest's stop verdict the other way. That is
//! the same bridge the runner uses for the same reason, and it is what keeps a
//! guest's early-stop meaningful rather than something noticed after the fact.

use crate::backend::BackendFactory;
use crate::infer::{InferBackend, InferRequest, InferResult};
use async_trait::async_trait;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// How much context to give a job when nothing says otherwise.
///
/// Modest on purpose. Context is the dominant consumer of memory per job — a
/// large window multiplies by every concurrent job — so this errs toward
/// running rather than toward filling the machine.
const DEFAULT_CONTEXT: u32 = 4096;

/// llama.cpp's global backend, initialised at most once per process.
///
/// `LlamaBackend::init` sets up process-wide state and must not run twice, but
/// several jobs may load models concurrently. A `OnceLock` makes the first
/// caller win and the rest wait, without callers having to coordinate.
fn backend() -> anyhow::Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("llama.cpp backend failed to initialise: {e}"))
}

/// Serves inference from a model loaded into this process.
pub struct LlamaCppBackend {
    /// Shared across jobs. The model is immutable once loaded and is `Sync`, so
    /// jobs share weights while each takes its own context — the same split the
    /// design calls for, and the reason a second job does not pay to load again.
    model: Arc<LlamaModel>,
    name: String,
    context_size: u32,
}

impl LlamaCppBackend {
    /// Load a GGUF model from `path`.
    ///
    /// Loading is eager and can take seconds for a large model. That is
    /// deliberate for a daemon: a missing or unreadable model should stop
    /// startup with a clear error rather than failing the first job that
    /// happens to arrive, and no job should pay the load cost.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            anyhow::bail!("no model file at {}", path.display());
        }

        let model = LlamaModel::load_from_file(backend()?, path, &LlamaModelParams::default())
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?;

        Ok(Self {
            model: Arc::new(model),
            // The file stem reads better in usage accounting than a full path.
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            context_size: DEFAULT_CONTEXT,
        })
    }

    /// Override the per-job context window.
    pub fn with_context_size(mut self, tokens: u32) -> Self {
        self.context_size = tokens;
        self
    }
}

/// Run generation to completion on the calling (blocking) thread.
///
/// Split out so the async wrapper stays about plumbing. `emit` returns whether
/// to keep going, which is how the guest's stop verdict arrives here.
fn generate(
    model: &LlamaModel,
    context_size: u32,
    prompt: &str,
    max_tokens: u32,
    mut emit: impl FnMut(&str) -> bool,
) -> anyhow::Result<InferResult> {
    let ctx_size = NonZeroU32::new(context_size.max(1)).expect("max(1) is non-zero");
    let mut ctx = model
        .new_context(
            backend()?,
            LlamaContextParams::default().with_n_ctx(Some(ctx_size)),
        )
        .map_err(|e| anyhow::anyhow!("creating llama.cpp context: {e}"))?;

    // Instruction-tuned models expect their chat template — the role markers
    // they were trained on. Ollama's /api/generate applies it for you; raw
    // llama.cpp does not, and feeding a bare string to a chat model produces
    // fluent, confident, degenerate output (typically the prompt echoed back on
    // a loop). That failure looks like a bad model rather than a missing
    // template, which is exactly why it is worth doing here.
    //
    // A model with no template is a base completion model, where the raw prompt
    // is correct.
    let prompt = match model.chat_template(None) {
        Ok(template) => {
            let message = LlamaChatMessage::new("user".to_string(), prompt.to_string())
                .map_err(|e| anyhow::anyhow!("building chat message: {e}"))?;
            // Not `unwrap_or_else(|_| raw_prompt)`. A model that *has* a template
            // and fails to apply it is broken, and falling back to the raw
            // prompt would resurrect exactly the degenerate output this template
            // handling was added to fix — while looking like it worked.
            model
                .apply_chat_template(&template, &[message], true)
                .map_err(|e| anyhow::anyhow!("applying the model's chat template: {e}"))?
        }
        // No template means a base completion model, where the raw prompt is
        // correct rather than a fallback.
        Err(_) => prompt.to_string(),
    };

    let tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .map_err(|e| anyhow::anyhow!("tokenizing prompt: {e}"))?;
    let tokens_in = tokens.len() as u32;

    if tokens_in >= context_size {
        anyhow::bail!(
            "prompt is {tokens_in} tokens but the context window is {context_size}; \
             raise the context size or shorten the prompt"
        );
    }

    // Feed the whole prompt, asking for logits only on the final token — the
    // earlier positions exist to build the KV cache, not to be sampled from.
    let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
    let last = tokens.len().saturating_sub(1);
    for (i, token) in tokens.into_iter().enumerate() {
        batch.add(token, i as i32, &[0], i == last)?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| anyhow::anyhow!("decoding prompt: {e}"))?;

    // Greedy sampling keeps a job reproducible, which is what makes its failures
    // investigable — but greedy alone gets stuck in loops, repeating a sentence
    // until it hits max_tokens. The repetition penalty breaks those cycles while
    // leaving the result deterministic, so both properties hold at once.
    //
    // Temperature and top-p belong in the spec, once there is somewhere to put
    // them; until then, determinism is the better default.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::penalties(64, 1.1, 0.0, 0.0),
        LlamaSampler::greedy(),
    ]);

    let mut text = String::new();
    let mut tokens_out = 0u32;
    let mut pos = last as i32 + 1;

    // A stateful decoder, not a per-token conversion: a single token can carry
    // part of a multi-byte character, so decoding each one independently would
    // produce replacement characters at exactly the seams where a model emits
    // non-ASCII text. The decoder carries the partial bytes across calls.
    let mut decoder = encoding_rs::UTF_8.new_decoder();

    while tokens_out < max_tokens {
        let token = sampler.sample(&ctx, -1);
        sampler.accept(token);

        if model.is_eog_token(token) {
            break;
        }

        // Not `unwrap_or_default()`. An empty piece from a decode failure is
        // indistinguishable from a token that legitimately renders to nothing,
        // so swallowing it silently truncates output mid-generation and returns
        // a shorter answer that looks complete.
        let piece = model
            .token_to_piece(token, &mut decoder, false, None)
            .map_err(|e| anyhow::anyhow!("decoding generated token: {e}"))?;
        tokens_out += 1;
        text.push_str(&piece);

        if !emit(&piece) {
            break;
        }

        batch.clear();
        batch.add(token, pos, &[0], true)?;
        pos += 1;
        ctx.decode(&mut batch)
            .map_err(|e| anyhow::anyhow!("decoding token: {e}"))?;
    }

    Ok(InferResult {
        text,
        tokens_in,
        tokens_out,
    })
}

#[async_trait]
impl InferBackend for LlamaCppBackend {
    async fn infer(
        &self,
        req: InferRequest<'_>,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        // llama.cpp does support multimodal (mtmd), but wiring it needs a
        // projector model alongside the weights and a different decode path.
        // Refusing is the honest answer until that exists: silently dropping the
        // images would answer a question about nothing.
        if !req.images.is_empty() {
            anyhow::bail!(
                "the embedded llama.cpp backend does not accept images yet; \
                 use the `ollama` provider with a vision model"
            );
        }
        // Tokens travel out over a channel and the stop verdict travels back via
        // a flag, because the generation loop runs on a blocking thread and
        // cannot hold a borrow of `on_token`.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let (model, stop, prompt) = (self.model.clone(), stop.clone(), req.prompt.to_string());
            let (context_size, max_tokens) = (self.context_size, req.max_tokens);
            tokio::task::spawn_blocking(move || {
                generate(&model, context_size, &prompt, max_tokens, |piece| {
                    tx.send(piece.to_string()).is_ok() && !stop.load(Ordering::Relaxed)
                })
            })
        };

        // Drain tokens as they are produced rather than after the fact, so a
        // stop verdict can still end generation early.
        tokio::pin!(handle);
        loop {
            tokio::select! {
                biased;
                Some(piece) = rx.recv() => {
                    if !on_token(&piece) {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                joined = &mut handle => {
                    // Forward anything produced in the same instant as the last.
                    while let Ok(piece) = rx.try_recv() {
                        on_token(&piece);
                    }
                    return joined.map_err(|e| anyhow::anyhow!("inference thread failed: {e}"))?;
                }
            }
        }
    }

    fn model_name(&self) -> String {
        self.name.clone()
    }
}

/// Builds [`LlamaCppBackend`], registered as the `llamacpp` provider.
pub struct LlamaCppFactory;

impl BackendFactory for LlamaCppFactory {
    fn provider(&self) -> &'static str {
        "llamacpp"
    }

    fn describe(&self) -> &'static str {
        "an embedded llama.cpp; target is a path to a .gguf model file"
    }

    fn build(&self, target: &str) -> anyhow::Result<Arc<dyn InferBackend>> {
        if target.is_empty() {
            anyhow::bail!("a path to a .gguf model file is required");
        }
        Ok(Arc::new(LlamaCppBackend::load(target)?))
    }
}
