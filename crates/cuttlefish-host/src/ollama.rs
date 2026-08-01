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

use crate::infer::{InferBackend, InferResult};
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
        }
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
        prompt: &str,
        max_tokens: u32,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        let response = self
            .client
            .post(format!("{}/api/generate", self.host))
            .json(&serde_json::json!({
                "model": self.model,
                "prompt": prompt,
                "stream": true,
                "options": { "num_predict": max_tokens },
            }))
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

                if !chunk.response.is_empty() {
                    text.push_str(&chunk.response);
                    tokens_out += 1;
                    if !on_token(&chunk.response) {
                        // The guest asked to stop. Dropping the stream closes
                        // the connection, which is how Ollama learns to stop
                        // generating — there is no separate cancel call.
                        break 'outer;
                    }
                }

                if chunk.done {
                    // The final message carries authoritative counts; prefer
                    // them over the tokens counted above, which only sees
                    // non-empty responses.
                    tokens_in = chunk.prompt_eval_count.unwrap_or(0);
                    tokens_out = chunk.eval_count.unwrap_or(tokens_out);
                    break 'outer;
                }
            }
        }

        Ok(InferResult {
            text,
            tokens_in,
            tokens_out,
        })
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }
}

/// Builds [`OllamaBackend`], registered as the `ollama` provider.
pub struct OllamaFactory;

impl crate::backend::BackendFactory for OllamaFactory {
    fn provider(&self) -> &'static str {
        "ollama"
    }

    fn describe(&self) -> &'static str {
        "a local Ollama instance; target is a model tag such as `llama3.2:1b`"
    }

    fn build(&self, target: &str) -> anyhow::Result<std::sync::Arc<dyn InferBackend>> {
        if target.is_empty() {
            anyhow::bail!("an Ollama model name is required, e.g. `llama3.2:1b`");
        }
        // Reachability is deliberately not checked here — see `BackendFactory`.
        Ok(std::sync::Arc::new(OllamaBackend::new(
            OllamaBackend::host_from_env(),
            target,
        )))
    }
}
