//! Inference served by a local [Ollama](https://ollama.com) instance.
//!
//! Ollama is the first real backend because it is the shortest path to a real
//! model: it is already an HTTP server that streams, so this is a client rather
//! than an embedding of llama.cpp. Embedding llama.cpp directly remains
//! worthwhile later — it removes a process boundary and gives direct control of
//! the KV cache — but it is a much larger commitment, and nothing above
//! [`InferBackend`] can tell the difference.
//!
//! # The wire format
//!
//! `POST /api/generate` with `stream: true` responds with newline-delimited
//! JSON — one object per token, then a final object carrying counts:
//!
//! ```text
//! {"model":"llama3.2:1b","response":"Hello","done":false}
//! {"model":"llama3.2:1b","response":" there","done":false}
//! {"model":"llama3.2:1b","response":"","done":true,"done_reason":"stop",
//!  "prompt_eval_count":28,"eval_count":3, ...}
//! ```
//!
//! Token-at-a-time delivery is what makes a guest's early-stop verdict
//! meaningful: the host can act on it while generation is still running rather
//! than after the fact.
//!
//! # Pinned generation
//!
//! Plain targets such as `Ollama "qwen3.8:27b"` keep the original behaviour.
//! A JSON target opts into a required digest pin and explicit generation controls:
//!
//! ```text
//! model = Ollama "{\"model\":\"qwen3.8:27b\",\"expected_digest\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"seed\":42,\"temperature\":0.2,\"format\":\"json\",\"think\":false}";
//! ```
//!
//! Replace the example digest with the actual 64-digit digest from
//! [`GET /api/tags`](https://docs.ollama.com/api/tags). Each configured generation
//! checks that mapping before and after inference and checks the response model.
//! Usage records the requested `tag@sha256:digest`; successful inference means
//! those checks passed. This is tag-mapping evidence, not atomic attestation of
//! the weights served. Resuming stored results does not re-attest their identity.
//! Keep the server and model files stable during a campaign.
//!
//! Supported [`generate`](https://docs.ollama.com/api/generate) controls are
//! integer `seed`, nonnegative finite `temperature`, `format` (`"json"` or a JSON
//! Schema object), and boolean `think`. Omitted controls retain Ollama defaults;
//! `num_predict` always comes from the request budget. Unknown fields fail.
//! Thinking and schema support remain model/server dependent; server errors
//! propagate. A seed does not guarantee identical output across versions or
//! hardware. Schema output still needs independent acceptance validation.

use crate::infer::{InferBackend, InferRequest, InferResult};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;

/// Where Ollama listens when nothing says otherwise.
pub const DEFAULT_HOST: &str = "http://localhost:11434";

/// One line of Ollama's streaming response.
///
/// Deliberately partial: Ollama sends more fields than this (timings, and a
/// `context` array that can run to thousands of integers). Naming only what is
/// used keeps the deserializer from being coupled to fields the project does
/// not care about, and avoids materializing that context array on every call.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
    /// Present when Ollama itself reports a problem — an unknown model, say.
    #[serde(default)]
    error: Option<String>,
}

/// Serves inference from a local Ollama instance.
pub struct OllamaBackend {
    client: reqwest::Client,
    host: String,
    model: String,
    config: Option<GenerationConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationConfig {
    model: String,
    expected_digest: String,
    seed: Option<i64>,
    temperature: Option<f64>,
    format: Option<serde_json::Value>,
    think: Option<bool>,
}

fn canonical_digest(value: &str) -> anyhow::Result<String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()),
        "Ollama digest must contain exactly 64 hexadecimal digits"
    );
    Ok(value.to_ascii_lowercase())
}

impl OllamaBackend {
    /// Target `model` on the Ollama instance at `host`.
    ///
    /// `model` is the name as Ollama knows it, `:tag` included.
    pub fn new(host: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            // A trailing slash here would produce `//api/generate`, which Ollama
            // rejects — easy to introduce via an environment variable.
            host: host.into().trim_end_matches('/').to_string(),
            model: model.into(),
            config: None,
        }
    }

    /// Build a legacy model tag or a validated JSON generation target.
    /// No network requests occur until inference.
    pub fn from_target(host: impl Into<String>, target: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !target.trim().is_empty(),
            "an Ollama model name is required"
        );
        if !target.trim_start().starts_with('{') {
            return Ok(Self::new(host, target));
        }
        let mut config: GenerationConfig = serde_json::from_str(target)
            .map_err(|e| anyhow::anyhow!("invalid Ollama generation configuration: {e}"))?;
        // Exact tag names avoid silently pinning one spelling while the server
        // resolves a different default tag in the generation request.
        anyhow::ensure!(
            !config.model.chars().any(char::is_whitespace)
                && config.model.rsplit('/').next().is_some_and(|last| {
                    last.split_once(':')
                        .is_some_and(|(name, tag)| !name.is_empty() && !tag.is_empty())
                }),
            "configured Ollama model requires an explicit name:tag"
        );
        config.expected_digest = canonical_digest(&config.expected_digest)?;
        if let Some(temperature) = config.temperature {
            anyhow::ensure!(
                temperature.is_finite() && temperature >= 0.0,
                "Ollama temperature must be finite and nonnegative"
            );
        }
        if let Some(format) = &config.format {
            if format.is_object() {
                jsonschema::validator_for(format)
                    .map_err(|e| anyhow::anyhow!("invalid Ollama format schema: {e}"))?;
            } else {
                anyhow::ensure!(
                    format.as_str() == Some("json"),
                    "Ollama format must be 'json' or a JSON Schema object"
                );
            }
        }
        let mut backend = Self::new(host, config.model.clone());
        backend.config = Some(config);
        Ok(backend)
    }

    async fn verify_digest(&self, config: &GenerationConfig) -> anyhow::Result<()> {
        let response = self
            .client
            .get(format!("{}/api/tags", self.host))
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        let models = body
            .get("models")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("Ollama /api/tags response has no models array"))?;
        let matches: Vec<_> = models
            .iter()
            .filter(|entry| {
                entry.get("name").and_then(serde_json::Value::as_str) == Some(self.model.as_str())
            })
            .collect();
        anyhow::ensure!(
            matches.len() == 1,
            "Ollama /api/tags must contain exactly one entry named {} (found {})",
            self.model,
            matches.len()
        );
        let digest = matches[0]
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Ollama tag {} has no digest", self.model))?;
        let digest = canonical_digest(digest)?;
        anyhow::ensure!(
            digest == config.expected_digest,
            "Ollama digest mismatch for {}: expected {}, observed {}",
            self.model,
            config.expected_digest,
            digest
        );
        Ok(())
    }

    /// Read `OLLAMA_HOST` if set, otherwise [`DEFAULT_HOST`].
    ///
    /// Named for the variable Ollama's own tooling uses, so an operator who has
    /// already pointed their CLI at a non-default instance does not have to
    /// configure this separately.
    pub fn host_from_env() -> String {
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string())
    }
}

#[async_trait]
impl InferBackend for OllamaBackend {
    async fn infer(
        &self,
        req: InferRequest<'_>,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        // Ollama takes images as base64 strings alongside the prompt. Whether
        // the *model* can use them is Ollama's business — asking a text-only
        // model produces a clear error from it, which is more useful than a
        // guess made here from a model name.
        let images: Vec<String> = req
            .images
            .iter()
            .map(|bytes| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(bytes)
            })
            .collect();

        let mut payload = serde_json::json!({
            "model": self.model,
            "prompt": req.prompt,
            "stream": true,
            "images": images,
            "options": { "num_predict": req.max_tokens },
        });
        if let Some(config) = &self.config {
            self.verify_digest(config).await?;
            if let Some(seed) = config.seed {
                payload["options"]["seed"] = seed.into();
            }
            if let Some(temperature) = config.temperature {
                payload["options"]["temperature"] = temperature.into();
            }
            if let Some(format) = &config.format {
                payload["format"] = format.clone();
            }
            if let Some(think) = config.think {
                payload["think"] = think.into();
            }
        }
        let response = self
            .client
            .post(format!("{}/api/generate", self.host))
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                // The overwhelmingly likely cause is that Ollama is not running,
                // and reqwest's own message does not say so.
                anyhow::anyhow!(
                    "could not reach Ollama at {} ({e}). Is it running? \
                     Set OLLAMA_HOST to point elsewhere.",
                    self.host
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {body}");
        }

        let mut text = String::new();
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        let mut terminal = false;
        let mut stopped = false;

        // Responses are newline-delimited JSON, and a chunk boundary need not
        // fall on a newline — a line can arrive split across two chunks, and one
        // chunk can carry several lines. Buffering and splitting on '\n' is what
        // makes this correct rather than usually-correct.
        let mut stream = response.bytes_stream();
        let mut buf = String::new();

        'outer: while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));

            while let Some(newline) = buf.find('\n') {
                // Take everything before the newline; leave the remainder in the
                // buffer as the start of the next line.
                let line: String = buf.drain(..=newline).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let chunk: Chunk = serde_json::from_str(line).map_err(|e| {
                    anyhow::anyhow!("malformed response from Ollama: {e} in {line}")
                })?;

                if let Some(error) = chunk.error {
                    anyhow::bail!("Ollama error: {error}");
                }
                if self.config.is_some() {
                    anyhow::ensure!(
                        chunk.model.as_deref() == Some(self.model.as_str()),
                        "Ollama response model {:?} does not match configured {}",
                        chunk.model,
                        self.model
                    );
                }

                if !chunk.response.is_empty() {
                    text.push_str(&chunk.response);
                    tokens_out += 1;
                    if !on_token(&chunk.response) {
                        stopped = true;
                        // The guest asked to stop. Dropping the stream closes
                        // the connection, which is how Ollama learns to stop
                        // generating — there is no separate cancel call.
                        break 'outer;
                    }
                }

                if chunk.done {
                    terminal = true;
                    // The final message carries authoritative counts; prefer
                    // them over the tokens counted above, which only sees
                    // non-empty responses.
                    tokens_in = chunk.prompt_eval_count.unwrap_or(0);
                    tokens_out = chunk.eval_count.unwrap_or(tokens_out);
                    break 'outer;
                }
            }
        }

        if let Some(config) = &self.config {
            anyhow::ensure!(
                terminal || stopped,
                "Ollama stream ended without a terminal response"
            );
            self.verify_digest(config).await?;
        }

        Ok(InferResult {
            text,
            tokens_in,
            tokens_out,
        })
    }

    fn model_name(&self) -> String {
        match &self.config {
            Some(config) => format!("{}@sha256:{}", self.model, config.expected_digest),
            None => self.model.clone(),
        }
    }

    /// True, but the honest claim is narrower than the signature allows:
    /// whether images work depends on the *model*, not on this backend, and
    /// this type does not know which model it is pointed at until a request is
    /// made.
    ///
    /// Forwarding is nonetheless the better behaviour, because Ollama answers
    /// the question authoritatively and specifically — HTTP 400 with
    /// "Multimodal data provided, but model does not support multimodal
    /// requests" (verified against llama3.2:1b). Guessing here from a model
    /// name would produce a worse message and a new way to be wrong, since the
    /// set of vision models changes without this code changing.
    fn supports_images(&self) -> bool {
        true
    }

    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::ensure!(self.config.is_none(),
            "Ollama generation configuration is not an embedding target; use a plain embedding model tag");
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // `/api/embed` (not the older `/api/embeddings`) because it takes an
        // array and answers with one vector per input. The legacy endpoint
        // is one text per request, which on a corpus is the difference
        // between minutes and hours.
        let response = self
            .client
            .post(format!("{}/api/embed", self.host))
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("embedding via {}: {e}", self.host))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("reading the embedding response: {e}"))?;
        if !status.is_success() {
            anyhow::bail!("embedding with `{}`: {status}: {body}", self.model);
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("embedding response was not JSON: {e}: {body}"))?;
        let rows = parsed
            .get("embeddings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("embedding response had no `embeddings` array: {body}")
            })?;

        // One vector per input, in order. A mismatch means the rows can no
        // longer be paired with their texts, and pairing them wrongly is
        // worse than failing: every downstream similarity would be against
        // the wrong content.
        if rows.len() != texts.len() {
            anyhow::bail!(
                "asked `{}` for {} embeddings and got {} — refusing to pair vectors with \
                 texts by guesswork",
                self.model,
                texts.len(),
                rows.len()
            );
        }

        let mut out = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let vector: Vec<f32> = row
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("embedding {i} was not an array: {row}"))?
                .iter()
                .map(|v| v.as_f64().unwrap_or(f64::NAN) as f32)
                .collect();
            if vector.is_empty() || vector.iter().any(|v| v.is_nan()) {
                anyhow::bail!(
                    "embedding {i} from `{}` is empty or contains non-numbers; storing it \
                     would poison every similarity computed against it",
                    self.model
                );
            }
            out.push(vector);
        }
        Ok(out)
    }

    fn supports_embeddings(&self) -> bool {
        self.config.is_none()
    }
}

/// Builds [`OllamaBackend`], registered as the `ollama` provider.
pub struct OllamaFactory;

impl crate::backend::BackendFactory for OllamaFactory {
    fn provider(&self) -> &'static str {
        "ollama"
    }

    fn describe(&self) -> &'static str {
        "a local Ollama instance; target is a model tag or a JSON generation configuration with expected_digest"
    }

    fn build(&self, target: &str) -> anyhow::Result<std::sync::Arc<dyn InferBackend>> {
        if target.is_empty() {
            anyhow::bail!("an Ollama model name is required, e.g. `llama3.2:1b`");
        }
        // Reachability is deliberately not checked here — see `BackendFactory`.
        Ok(std::sync::Arc::new(OllamaBackend::from_target(
            OllamaBackend::host_from_env(),
            target,
        )?))
    }
}
