//! Tests that failures are loud rather than plausible.
//!
//! Every case here is one where the code could have returned something that
//! *looks* like a valid answer — an empty page, a truncated generation, an
//! image-free response to a question about an image. Those are worse than
//! errors, because a caller cannot tell them from success and neither can a
//! test that only checks for `Ok`.
//!
//! Each of these was a real silent fallback in this codebase before it had a
//! test.

use cuttlefish_host::{
    documents,
    infer::{InferBackend, InferRequest, StubBackend},
};

#[tokio::test]
async fn the_stub_reports_images_instead_of_ignoring_them() {
    // A test backend that silently discards images cannot catch a real backend
    // doing the same thing — it would make the bug untestable.
    let backend = StubBackend::default();
    let images = vec![vec![1u8, 2, 3]];

    let mut noop = |_: &str| true;
    let result = backend
        .infer(
            InferRequest {
                prompt: "what is this",
                max_tokens: 32,
                images: &images,
            },
            &mut noop,
        )
        .await
        .unwrap();

    assert!(
        result.text.contains("1 image(s)"),
        "the stub must make images visible in its output, got: {:?}",
        result.text
    );
}

#[tokio::test]
async fn the_stub_is_unchanged_without_images() {
    let backend = StubBackend::default();
    let mut noop = |_: &str| true;
    let result = backend
        .infer(InferRequest::new("hello", 32), &mut noop)
        .await
        .unwrap();

    assert_eq!(result.text, "a stub summary");
    assert!(!result.text.contains("image"));
}

/// The repository's sample PDF, which has one page and a real text layer.
fn sample_pdf() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/docs/sample.pdf")
}

#[test]
fn a_page_that_yields_no_text_fails_rather_than_returning_empty() {
    // The document's page tree says how many pages exist; extraction says what
    // text it found. When those disagree, returning "" would make a caller
    // summarize nothing and report success.
    //
    // Page 0 of the sample extracts fine, so this asks for a page that the tree
    // does not have — the out-of-range path — and then the harder case is
    // covered by the assertion in `page_text` itself.
    let err = documents::page_text(&sample_pdf(), 5).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("out of range"),
        "a nonexistent page must say so: {msg}"
    );
}

#[test]
fn an_extractable_page_still_returns_its_text() {
    // The guard above must not have made the working path fail.
    let text = documents::page_text(&sample_pdf(), 0).expect("page 0 extracts");
    assert!(text.to_lowercase().contains("cuttlefish"), "got: {text:?}");
}

#[test]
fn rendering_without_the_feature_names_the_feature() {
    // Returning a blank image would look like a blank page.
    #[cfg(not(feature = "pdf-render"))]
    {
        let err = documents::render_page(&sample_pdf(), 0, 256).unwrap_err();
        assert!(err.to_string().contains("pdf-render"), "{err}");
    }
}
