//! `cuttlefish models list` — what local models are available, and what's
//! already known about them, so an agent doesn't have to hand-probe Ollama
//! every session to discover things like "this pulled model is a reasoning
//! model that burns its whole token budget on `<think>` output and returns
//! an empty reply at a modest `max_tokens`."

use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
pub struct ModelInfo {
    /// The tag Ollama knows it by, e.g. `qwen3.5:latest` — pass this
    /// straight into a spec's `model = Ollama "..."`.
    pub name: String,
    pub size_bytes: u64,
    /// Best-effort, from [`classify_reasoning`] — never guessed `false`
    /// for an unrecognized family, only `true`, `false`, or unknown
    /// (`null` in the JSON this prints).
    pub reasoning: Option<bool>,
}

/// Best-effort classification of whether a model defaults to emitting
/// `<think>`-style reasoning tokens, from a small built-in table of
/// well-known families matched against the tag name.
///
/// **Not exhaustive, not verified per quantization/finetune, and
/// deliberately asymmetric**: an unrecognized family is `None` (unknown),
/// never guessed as `Some(false)`. A false "this is not a reasoning
/// model" is worse than an honest "don't know" — the same "throw, don't
/// silently default" reasoning `cuttlefish-author`'s determinism rules and
/// `validate-json` both follow elsewhere in this codebase. A finetune or
/// an explicitly non-thinking variant of a listed family (e.g. an
/// `-instruct` suffix on a family that otherwise defaults to thinking) can
/// still be misclassified — this table names the common case, not every
/// case. Update it as new families become common enough to be worth
/// naming; it exists to save the *already-known, obvious* cases from being
/// rediscovered by hand every session, not to be a complete oracle.
fn classify_reasoning(name: &str) -> Option<bool> {
    let lower = name.to_lowercase();

    // An embedding model isn't a generation model at all -- "reasoning"
    // (does infer() burn tokens on <think> output) is a category error for
    // it, not a `false`. Checked first: `qwen3-embedding` would otherwise
    // match the `qwen3` family pattern below and report `true`, which is
    // wrong in a different way than an ordinary misclassification -- the
    // question doesn't apply, the model is never used via infer() at all.
    if lower.contains("embed") {
        return None;
    }

    const KNOWN_REASONING: &[&str] = &[
        "deepseek-r1",
        "qwq",
        "qwen3", // the qwen3 family defaults to a thinking mode as of writing
        "phi4-reasoning",
        "magistral",
        "marco-o1",
    ];
    const KNOWN_NON_REASONING: &[&str] = &[
        "llama3",
        "llama2",
        "gemma",
        "mistral",
        "phi3",
        "phi4-mini",
        "codellama",
        "starcoder",
    ];

    if KNOWN_REASONING
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        Some(true)
    } else if KNOWN_NON_REASONING
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        Some(false)
    } else {
        None
    }
}

#[derive(serde::Deserialize)]
struct TagsResponse {
    models: Vec<TagEntry>,
}

#[derive(serde::Deserialize)]
struct TagEntry {
    name: String,
    #[serde(default)]
    size: u64,
}

/// Query `{host}/api/tags` for every model Ollama has pulled locally, and
/// classify each one.
pub async fn list_ollama_models(host: &str) -> anyhow::Result<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let response = reqwest::get(&url)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to Ollama at {host}: {e} (is it running?)"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("Ollama at {host} returned an error: {e}"))?
        .json::<TagsResponse>()
        .await
        .map_err(|e| anyhow::anyhow!("parsing Ollama's response from {host}: {e}"))?;

    Ok(response
        .models
        .into_iter()
        .map(|entry| ModelInfo {
            reasoning: classify_reasoning(&entry.name),
            name: entry.name,
            size_bytes: entry.size,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_reasoning_family_is_classified_true() {
        assert_eq!(classify_reasoning("qwen3.5:latest"), Some(true));
        assert_eq!(classify_reasoning("deepseek-r1:8b"), Some(true));
    }

    #[test]
    fn a_known_non_reasoning_family_is_classified_false() {
        assert_eq!(classify_reasoning("llama3.2:3b"), Some(false));
        assert_eq!(classify_reasoning("gemma4:latest"), Some(false));
    }

    #[test]
    fn an_unrecognized_family_is_unknown_not_guessed_false() {
        assert_eq!(classify_reasoning("some-brand-new-model:latest"), None);
    }

    #[test]
    fn an_embedding_model_is_unknown_not_misclassified_via_its_base_family() {
        // qwen3-embedding would otherwise match the qwen3 family pattern
        // and wrongly report `true` -- an embedding model isn't a
        // generation model, so "reasoning" doesn't apply to it at all.
        assert_eq!(classify_reasoning("qwen3-embedding:4b"), None);
        assert_eq!(classify_reasoning("mxbai-embed-large:latest"), None);
    }
}
