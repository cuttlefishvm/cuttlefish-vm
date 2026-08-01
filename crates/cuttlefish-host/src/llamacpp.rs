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
//! # Model compatibility, learned the hard way
//!
//! A GGUF pulled by Ollama is **not** necessarily loadable here. Ollama ships
//! its own llama.cpp fork carrying architectures upstream does not have, so a
//! standard architecture works (`llama3.2` loads fine) and anything Ollama added
//! does not:
//!
//! - `glm-ocr` — `unknown model architecture: 'glmocr'`
//! - `gemma4:e2b` — `wrong number of tensors; expected 2012, got 601`
//!
//! Models built for upstream llama.cpp — SmolVLM, Qwen2-VL, LLaVA — load
//! normally. Worth knowing before pointing this provider at an Ollama blob and
//! concluding the backend is broken.
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
use llama_cpp_2::mtmd::{MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputText};
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
/// `LlamaBackend::init` sets up process-wide state and returns
/// `BackendAlreadyInitialized` if it runs twice, but several jobs may load
/// models concurrently. A `OnceLock` makes the first caller win and the rest
/// wait, without callers having to coordinate.
///
/// Public because it is the *only* correct way to obtain one in this process:
/// anything calling `LlamaBackend::init` itself — a test, an embedder — will
/// fail as soon as a model has already been loaded. Handing out the shared one
/// removes that footgun rather than documenting it.
pub fn shared_backend() -> anyhow::Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("llama.cpp backend failed to initialise: {e}"))
}

/// Serves inference from a model loaded into this process.
pub struct LlamaCppBackend {
    /// The multimodal projector, when this model has one.
    ///
    /// Held as a path rather than an open [`MtmdContext`] because that context
    /// borrows the model and is not `Sync`; it is built per request, alongside
    /// the per-job llama context it has to share a lifetime with.
    mmproj: Option<std::path::PathBuf>,
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

        let model =
            LlamaModel::load_from_file(shared_backend()?, path, &LlamaModelParams::default())
                .map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?;

        // A projector sitting next to the weights is the convention llama.cpp
        // and the GGUF publishers both use — `mmproj-<name>.gguf` beside
        // `<name>.gguf`. Finding it automatically means a spec names one path
        // for a vision model exactly as it does for a text one.
        let mmproj = find_mmproj(path);
        if let Some(found) = &mmproj {
            eprintln!("llamacpp: using multimodal projector {}", found.display());
        }

        Ok(Self {
            mmproj,
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

    /// Point at a projector explicitly, rather than relying on the sibling
    /// convention.
    pub fn with_mmproj(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.mmproj = Some(path.into());
        self
    }

    /// Whether this model was loaded with a projector.
    pub fn has_projector(&self) -> bool {
        self.mmproj.is_some()
    }
}

/// Look for a multimodal projector beside a model file.
///
/// Matches `mmproj*.gguf` in the same directory, which is what both llama.cpp's
/// own tooling and the GGUF publishers produce. Returns the first match in
/// sorted order so the choice is stable rather than filesystem-dependent — an
/// unstable pick would make a job non-reproducible for reasons nobody could see.
fn find_mmproj(model: &Path) -> Option<std::path::PathBuf> {
    let dir = model.parent()?;
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.starts_with("mmproj") && name.ends_with(".gguf")
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// Decode encoded image bytes into the flat RGB24 buffer mtmd expects.
///
/// mtmd takes raw pixels, not a PNG or a JPEG, so this is where an image
/// actually gets decoded. Failing here is deliberate rather than substituting a
/// blank image: a blank image produces a confident description of nothing.
fn decode_rgb(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("decoding an image for the vision model: {e}"))?;
    let rgb = decoded.to_rgb8();
    Ok((rgb.width(), rgb.height(), rgb.into_raw()))
}

/// Run generation to completion on the calling (blocking) thread.
///
/// Split out so the async wrapper stays about plumbing. `emit` returns whether
/// to keep going, which is how the guest's stop verdict arrives here.
#[allow(clippy::too_many_arguments)]
fn generate(
    model: &LlamaModel,
    mmproj: Option<&Path>,
    context_size: u32,
    prompt: &str,
    images: &[Vec<u8>],
    max_tokens: u32,
    mut emit: impl FnMut(&str) -> bool,
) -> anyhow::Result<InferResult> {
    let ctx_size = NonZeroU32::new(context_size.max(1)).expect("max(1) is non-zero");
    let mut ctx = model
        .new_context(
            shared_backend()?,
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

    // Two ways to fill the context: mtmd when there are images, plain
    // tokenization otherwise. Both leave `pos` at the first position to generate
    // from, and `tokens_in` counting what the prompt cost.
    let mut batch = LlamaBatch::new(context_size as usize, 1);
    let (tokens_in, mut pos) = if images.is_empty() {
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
        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            batch.add(*token, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| anyhow::anyhow!("decoding prompt: {e}"))?;

        (tokens_in, last as i32 + 1)
    } else {
        let mmproj = mmproj.ok_or_else(|| {
            anyhow::anyhow!(
                "this job supplied images, but no multimodal projector was found \
                 beside the model. A vision model needs an `mmproj-*.gguf` in the \
                 same directory as its weights."
            )
        })?;

        let mtmd = MtmdContext::init_from_file(
            &mmproj.to_string_lossy(),
            model,
            &MtmdContextParams::default(),
        )
        .map_err(|e| {
            anyhow::anyhow!("loading the multimodal projector {}: {e}", mmproj.display())
        })?;

        // mtmd splices images in wherever its marker appears, so the marker has
        // to be in the text — one per image, prepended, because a caller's
        // prompt has no reason to know about mtmd's marker syntax.
        let marker = llama_cpp_2::mtmd::mtmd_default_marker();
        let mut bitmaps = Vec::with_capacity(images.len());
        for bytes in images {
            let (w, h, rgb) = decode_rgb(bytes)?;
            bitmaps.push(
                MtmdBitmap::from_image_data(w, h, &rgb)
                    .map_err(|e| anyhow::anyhow!("preparing an image for the model: {e}"))?,
            );
        }
        let refs: Vec<&MtmdBitmap> = bitmaps.iter().collect();

        let text = format!("{}{prompt}", marker.repeat(images.len()));
        let chunks = mtmd
            .tokenize(
                MtmdInputText {
                    text,
                    add_special: true,
                    parse_special: true,
                },
                &refs,
            )
            .map_err(|e| anyhow::anyhow!("tokenizing the prompt with images: {e}"))?;

        // eval_chunks encodes each image through the projector and decodes the
        // text around it, leaving the context primed. It returns the position to
        // continue generating from.
        let n_past = chunks
            .eval_chunks(&mtmd, &ctx, 0, 0, context_size as i32, true)
            .map_err(|e| anyhow::anyhow!("evaluating the prompt with images: {e}"))?;

        (n_past as u32, n_past)
    };

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
        // Fail early with a message naming the fix, rather than letting mtmd
        // report a missing projector from deep inside a worker thread.
        if !req.images.is_empty() && self.mmproj.is_none() {
            anyhow::bail!(
                "this job supplied images, but no multimodal projector was found \
                 beside {}. A vision model needs an `mmproj-*.gguf` in the same \
                 directory as its weights.",
                self.name
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
            let mmproj = self.mmproj.clone();
            let images: Vec<Vec<u8>> = req.images.to_vec();
            tokio::task::spawn_blocking(move || {
                generate(
                    &model,
                    mmproj.as_deref(),
                    context_size,
                    &prompt,
                    &images,
                    max_tokens,
                    |piece| tx.send(piece.to_string()).is_ok() && !stop.load(Ordering::Relaxed),
                )
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

    /// Only when a projector was found. Without one the model has no vision
    /// tower to route images through, and claiming otherwise would let the
    /// runner hand over images this backend must then refuse deeper in.
    fn supports_images(&self) -> bool {
        self.mmproj.is_some()
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
