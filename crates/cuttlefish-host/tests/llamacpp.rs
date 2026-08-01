//! Tests for the embedded llama.cpp backend.
//!
//! Only built when the `llamacpp` feature is on. Most of these need a real GGUF
//! model, which is far too large to check in, so they read a path from
//! `CUTTLEFISH_TEST_GGUF` and skip when it is unset — a skipped test says so
//! rather than passing silently:
//!
//! ```console
//! $ export CUTTLEFISH_TEST_GGUF=/path/to/model.gguf
//! $ cargo test -p cuttlefish-host --features llamacpp --test llamacpp -- --ignored
//! ```
//!
//! Ollama stores its models as GGUF, so an already-pulled model works:
//! `~/.ollama/models/blobs/sha256-...` for the layer named in its manifest.
//!
//! Assertions are about *shape*, never wording. A language model is not a
//! deterministic function of its prompt, and a test that expects particular
//! words fails for reasons that have nothing to do with this code.

#![cfg(feature = "llamacpp")]

use cuttlefish_core::spec::ModelRef;
use cuttlefish_host::{
    backend::Registry,
    infer::{InferBackend, InferRequest},
    llamacpp::{LlamaCppBackend, LlamaCppFactory},
};

/// The model under test, or `None` when the environment does not name one.
fn test_model() -> Option<String> {
    std::env::var("CUTTLEFISH_TEST_GGUF")
        .ok()
        .filter(|p| !p.is_empty())
}

/// Skip with an explanation rather than passing vacuously.
macro_rules! model_or_skip {
    () => {
        match test_model() {
            Some(path) => path,
            None => {
                eprintln!("skipping: set CUTTLEFISH_TEST_GGUF to a .gguf model file");
                return;
            }
        }
    };
}

#[test]
fn the_provider_is_registered_when_the_feature_is_on() {
    // The registry contract: enabling the feature adds a provider and changes
    // nothing else.
    assert!(Registry::with_builtins().providers().contains(&"llamacpp"));
}

#[test]
fn a_missing_model_file_is_rejected_with_the_path() {
    let err = match LlamaCppBackend::load("/definitely/not/a/model.gguf") {
        Ok(_) => panic!("a missing model file must not load"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(msg.contains("/definitely/not/a/model.gguf"), "{msg}");
}

#[test]
fn the_factory_requires_a_target() {
    use cuttlefish_host::backend::BackendFactory;
    let err = match LlamaCppFactory.build("") {
        Ok(_) => panic!("an empty target must be rejected"),
        Err(e) => e,
    };
    assert!(err.to_string().contains(".gguf"), "{err}");
}

#[test]
fn resolving_through_the_registry_reports_a_bad_path() {
    let err = Registry::with_builtins()
        .resolve(&ModelRef::new("llamacpp", "/no/such/model.gguf"))
        .err()
        .expect("a missing model must not resolve");
    let msg = err.to_string();
    assert!(
        msg.contains("llamacpp"),
        "the provider must be named: {msg}"
    );
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a .gguf model"]
async fn generates_text_from_a_real_model() {
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path).expect("model should load");

    let mut streamed = Vec::new();
    let mut sink = |piece: &str| {
        streamed.push(piece.to_string());
        true
    };

    let result = backend
        .infer(InferRequest::new("Count: one two three", 24), &mut sink)
        .await
        .expect("generation should succeed");

    assert!(!result.text.is_empty(), "the model produced no text");
    assert!(
        !streamed.is_empty(),
        "tokens must stream, not just be returned"
    );
    assert_eq!(
        streamed.concat(),
        result.text,
        "streamed pieces must reconstruct the returned text exactly"
    );
    assert!(result.tokens_in > 0, "prompt tokens must be counted");
    assert!(result.tokens_out > 0, "generated tokens must be counted");
    assert!(
        result.tokens_out <= 24,
        "max_tokens must be respected, got {}",
        result.tokens_out
    );
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a .gguf model"]
async fn a_stop_verdict_ends_generation_early() {
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path).expect("model should load");

    let mut count = 0usize;
    let mut sink = |_: &str| {
        count += 1;
        // Stop after three tokens.
        count < 3
    };

    let result = backend
        .infer(
            InferRequest::new("Write a long paragraph about the sea.", 200),
            &mut sink,
        )
        .await
        .expect("generation should succeed");

    assert!(
        result.tokens_out < 200,
        "stop must cut generation short, got {} tokens",
        result.tokens_out
    );
    assert!(
        count <= 4,
        "generation should end promptly, saw {count} tokens"
    );
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a .gguf model"]
async fn greedy_sampling_makes_a_job_reproducible() {
    // Determinism is a design choice, not an accident: a reproducible job is
    // one whose failures can be investigated. If sampling ever gains
    // temperature, this test is what will notice.
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path).expect("model should load");

    let mut noop = |_: &str| true;
    let first = backend
        .infer(InferRequest::new("The capital of France is", 8), &mut noop)
        .await
        .unwrap();
    let second = backend
        .infer(InferRequest::new("The capital of France is", 8), &mut noop)
        .await
        .unwrap();

    assert_eq!(
        first.text, second.text,
        "greedy sampling must be deterministic"
    );
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a .gguf model"]
async fn a_prompt_larger_than_the_context_is_refused_clearly() {
    // Failing with an explanation beats llama.cpp's own behaviour here, which
    // is to abort the process.
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path)
        .expect("model should load")
        .with_context_size(64);

    let mut noop = |_: &str| true;
    let err = backend
        .infer(InferRequest::new(&"word ".repeat(200), 8), &mut noop)
        .await
        .expect_err("an oversized prompt must be refused");

    let msg = err.to_string();
    assert!(msg.contains("context window"), "{msg}");
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a .gguf model"]
async fn jobs_do_not_share_context() {
    // Each inference builds its own context, so nothing carries between them.
    // A shared context would let one job's history influence another's output —
    // both a correctness problem and a privacy one.
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path).expect("model should load");

    let mut noop = |_: &str| true;
    let primed = backend
        .infer(
            InferRequest::new("Remember the secret word: platypus.", 16),
            &mut noop,
        )
        .await
        .unwrap();
    let after = backend
        .infer(InferRequest::new("The capital of France is", 8), &mut noop)
        .await
        .unwrap();

    assert!(!primed.text.is_empty());
    assert!(
        !after.text.to_lowercase().contains("platypus"),
        "context leaked between jobs: {}",
        after.text
    );
}

/// Probe: can mtmd initialise from a GGUF whose projector is embedded?
///
/// Some multimodal GGUFs (glm-ocr among them) carry the vision tower and the
/// `mm.*` projector tensors in the same file as the text weights, rather than in
/// a separate mmproj. `mtmd_init_from_file` takes a projector path *and* a text
/// model, so the question is whether passing the same path for both works.
///
/// This exists to answer that before any backend code is written against it —
/// building multimodal support that cannot be verified would be exactly the
/// thing AGENTS.md says not to do.
///
/// # Result so far: not yet verifiable with locally available models
///
/// Neither vision model installed through Ollama loads in the llama.cpp that
/// `llama-cpp-sys-2` vendors:
///
/// - `glm-ocr` — `unknown model architecture: 'glmocr'`
/// - `gemma4:e2b` — `wrong number of tensors; expected 2012, got 601`
///
/// The lesson generalises beyond multimodal: **an Ollama blob is not
/// necessarily loadable by an arbitrary llama.cpp build.** Ollama ships its own
/// fork carrying architectures upstream does not have, so "point the llamacpp
/// provider at an Ollama blob" works for a standard architecture (llama3.2
/// does) and fails for anything Ollama added.
///
/// Verifying mtmd therefore needs a model built for upstream llama.cpp — a
/// Qwen2-VL, LLaVA, or SmolVLM GGUF with its mmproj — rather than whatever
/// Ollama happens to have pulled.
#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_VISION_GGUF pointing at a multimodal .gguf"]
async fn mtmd_can_initialise_from_an_embedded_projector() {
    let Some(path) = std::env::var("CUTTLEFISH_TEST_VISION_GGUF")
        .ok()
        .filter(|p| !p.is_empty())
    else {
        eprintln!("skipping: set CUTTLEFISH_TEST_VISION_GGUF");
        return;
    };

    use llama_cpp_2::mtmd::{MtmdContext, MtmdContextParams};

    let backend = llama_cpp_2::llama_backend::LlamaBackend::init().expect("backend");
    let model = llama_cpp_2::model::LlamaModel::load_from_file(
        &backend,
        &path,
        &llama_cpp_2::model::params::LlamaModelParams::default(),
    )
    .expect("the vision model should load as a text model");

    // A separate mmproj when one is named, otherwise the model file itself —
    // some GGUFs embed the projector.
    let mmproj = std::env::var("CUTTLEFISH_TEST_MMPROJ")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| path.clone());

    let params = MtmdContextParams::default();
    match MtmdContext::init_from_file(&mmproj, &model, &params) {
        Ok(_) => eprintln!("VERIFIED: mtmd initialised (mmproj={mmproj})"),
        Err(e) => panic!("mtmd could not initialise from {mmproj}: {e}"),
    }
}
