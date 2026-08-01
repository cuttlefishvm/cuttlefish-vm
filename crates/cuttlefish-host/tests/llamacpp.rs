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

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_VISION_GGUF with an mmproj beside it"]
async fn describes_an_image_through_the_embedded_projector() {
    let Some(path) = std::env::var("CUTTLEFISH_TEST_VISION_GGUF")
        .ok()
        .filter(|p| !p.is_empty())
    else {
        eprintln!("skipping: set CUTTLEFISH_TEST_VISION_GGUF");
        return;
    };

    let backend = LlamaCppBackend::load(&path).expect("the vision model should load");
    assert!(
        backend.has_projector(),
        "no mmproj-*.gguf found beside {path}; the sibling convention is what \
         makes a vision model usable from a spec naming one path"
    );
    assert!(backend.supports_images());

    // A 64x64 image with a solid black bar. Asserting on *shape* rather than
    // wording: a model is not a deterministic function, and a test expecting
    // particular words fails for reasons unrelated to this code.
    let png = {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut img = image::RgbImage::new(64, 64);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if (10..54).contains(&x) && (28..36).contains(&y) {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    };

    let images = vec![png];
    let mut streamed = Vec::new();
    let mut sink = |piece: &str| {
        streamed.push(piece.to_string());
        true
    };

    let result = backend
        .infer(
            cuttlefish_host::infer::InferRequest {
                prompt: "Describe this image in one sentence.",
                max_tokens: 40,
                images: &images,
            },
            &mut sink,
        )
        .await
        .expect("vision inference should succeed");

    eprintln!("VISION OUTPUT: {}", result.text);

    assert!(!result.text.is_empty(), "the model produced no text");
    assert!(!streamed.is_empty(), "tokens must stream");
    assert_eq!(streamed.concat(), result.text, "streamed text must match");
    assert!(
        result.tokens_in > 10,
        "an image should cost many prompt tokens, got {}",
        result.tokens_in
    );
}

#[tokio::test]
#[ignore = "requires CUTTLEFISH_TEST_GGUF pointing at a text-only .gguf"]
async fn a_text_only_model_refuses_images_by_name() {
    let path = model_or_skip!();
    let backend = LlamaCppBackend::load(&path).expect("model should load");
    if backend.has_projector() {
        eprintln!("skipping: this model has a projector, so it accepts images");
        return;
    }

    assert!(
        !backend.supports_images(),
        "without a projector the backend must not claim image support"
    );

    let images = vec![vec![0u8; 8]];
    let mut noop = |_: &str| true;
    let err = backend
        .infer(
            cuttlefish_host::infer::InferRequest {
                prompt: "what is this",
                max_tokens: 8,
                images: &images,
            },
            &mut noop,
        )
        .await
        .expect_err("a model without a projector must refuse images");

    assert!(err.to_string().contains("mmproj"), "{err}");
}
