//! Reading paged documents — PDFs today.
//!
//! A document can be read two ways, and which is right depends on the document
//! rather than on preference:
//!
//! - **Extract its text layer.** Cheap, exact, and works with any text model.
//!   Useless for a scanned page, which has no text layer at all.
//! - **Render pages to images** for a vision model. Works on anything a human
//!   could read, including scans, and preserves layout, tables, and figures that
//!   extraction flattens or drops. Far slower and needs a vision model.
//!
//! So the host reports what a document *offers* — page count, and whether a text
//! layer exists — and the block decides. That is why
//! [`MediaKind::Document`](cuttlefish_abi::MediaKind::Document) carries
//! `has_text_layer`: a block that checks it can take the cheap path when it
//! exists and the expensive one when it must, instead of silently extracting
//! nothing from a scan and summarizing the empty string.
//!
//! # Why rendering is optional
//!
//! Text extraction is pure Rust and always available. Rasterizing needs a PDF
//! renderer, which is a large native dependency, so it sits behind the
//! `pdf-render` feature. Without it, [`render_page`] fails with a message saying
//! exactly that rather than pretending the page is blank.

use std::path::Path;

/// What a document offers a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentInfo {
    /// How many pages it has.
    pub pages: u32,
    /// Whether any page carries extractable text.
    pub has_text_layer: bool,
}

/// Inspect a PDF without committing to reading all of it.
pub fn inspect(path: &Path) -> anyhow::Result<DocumentInfo> {
    let doc = lopdf::Document::load(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let pages = doc.get_pages().len() as u32;

    // "Has a text layer" is answered by extracting and looking, because the
    // alternative — inspecting font dictionaries — says a page *could* contain
    // text without saying whether it does. A scanned page often still carries
    // font resources from whatever produced it.
    let has_text_layer = pdf_extract::extract_text(path)
        .map(|text| text.chars().any(|c| c.is_alphanumeric()))
        .unwrap_or(false);

    Ok(DocumentInfo {
        pages,
        has_text_layer,
    })
}

/// How many pages the PDF's own page tree reports.
///
/// Separate from [`inspect`] on purpose: `inspect` also answers
/// `has_text_layer`, and the only honest way to answer that is to extract
/// the text and look. Calling it merely to learn a page count therefore
/// costs a full extraction — which is exactly the trap that made a page
/// walk quadratic even *after* the text itself was cached.
pub fn page_count(path: &Path) -> anyhow::Result<u32> {
    let doc = lopdf::Document::load(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    Ok(doc.get_pages().len() as u32)
}

/// Every character of text in the document, in one call.
///
/// This is what `pdf_extract` produces internally and what most callers
/// actually want. It exists as its own entry point because the only way to
/// ask for it used to be `page_text(handle, 0)`, which *reads* like "give me
/// the first page" and silently meant something else whenever the extractor
/// emitted no page breaks.
pub fn document_text(path: &Path) -> anyhow::Result<String> {
    pdf_extract::extract_text(path)
        .map_err(|e| anyhow::anyhow!("extracting text from {}: {e}", path.display()))
}

/// How many text segments [`page_text_from`] can actually address.
///
/// Deliberately not the same number as [`DocumentInfo::pages`], and that is
/// the entire point. `pages` comes from the PDF's page tree; this comes from
/// splitting the extracted text on form feeds, which is all `pdf_extract`
/// gives us to locate a page boundary with. For many real documents — every
/// PDF in the CMS section 1115 corpus, for instance — the extractor emits no
/// form feeds at all, so a 227-page document has exactly one addressable
/// segment.
///
/// Two numbers with the same name meaning different things is what made the
/// old failure so confusing. Now both are computable, so an error can name
/// them both.
pub fn text_segments(text: &str) -> usize {
    text.split('\u{c}').count()
}

/// Take one segment of already-extracted text, zero-based.
///
/// Separated from extraction so a caller reading a document page by page
/// pays for the extraction once rather than once per page. The old shape
/// re-extracted the whole document on every call, which made the natural
/// page-walk quadratic — a 342-page filing meant 342 full extractions, and
/// on a corpus of thousands that is not slow, it is unrunnable.
pub fn page_text_from(text: &str, page: u32, page_tree_count: u32) -> anyhow::Result<String> {
    let segments: Vec<&str> = text.split('\u{c}').collect();
    match segments.get(page as usize) {
        Some(found) => Ok(found.to_string()),
        // A document the extractor never split still has all its text, and
        // page zero is the only sensible place to hand it back.
        None if page == 0 => Ok(text.to_string()),
        // Returning an empty string here would be a silent wrong answer: the
        // caller would summarize nothing and report success.
        //
        // The message names both counts, because the difference between them
        // *is* the problem. Its predecessor said the page "may be scanned"
        // and to check `has_text_layer` — advice that sent at least one real
        // user toward replacing the extractor, when `has_text_layer` was
        // `true` and the text was right there in segment zero.
        None => anyhow::bail!(
            "page {page} is out of range for extracted text: this document exposes \
             {} addressable text segment(s), though its page tree reports {page_tree_count} \
             page(s). The extractor emitted no page break there, so the text for page \
             {page} is not separately addressable — read the whole document with \
             `document_text` instead, or render the page if it is genuinely scanned.",
            segments.len()
        ),
    }
}

/// Render one page to a PNG, zero-based.
///
/// Runs pdfium in a **subprocess**. pdfium segfaults on input other parsers
/// accept, and in-process that would kill the daemon and every job running
/// alongside it rather than failing the one job holding the bad PDF. See
/// [`crate::render_worker`].
///
/// Requires the `pdf-render` feature.
#[cfg(feature = "pdf-render")]
pub fn render_page(path: &Path, page: u32, width: u16) -> anyhow::Result<Vec<u8>> {
    crate::render_worker::render_page(path, page, width)
}

/// Render a page in *this* process. Only the render worker should call this.
///
/// Kept separate so the isolation is not accidentally bypassed: anything
/// reaching for `render_page` gets the safe path, and the unsafe one is named
/// in a way that says why it is not the default.
#[cfg(feature = "pdf-render")]
pub(crate) fn render_page_in_process(
    path: &Path,
    page: u32,
    width: u16,
) -> anyhow::Result<Vec<u8>> {
    use pdfium_render::prelude::*;

    // pdfium is a shared library loaded at runtime, not linked in.
    //
    // `bind_to_system_library` alone searches only the platform's default
    // loader paths, which is exactly where a Nix-provided library is *not*.
    // `PDFIUM_DYNAMIC_LIB_PATH` is the escape hatch — set by this project's dev
    // shell — with the system library as the fallback for a conventional
    // install.
    let bindings = match std::env::var("PDFIUM_DYNAMIC_LIB_PATH") {
        Ok(dir) if !dir.is_empty() => {
            Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))
                .or_else(|_| Pdfium::bind_to_system_library())
        }
        _ => Pdfium::bind_to_system_library(),
    }
    .map_err(|e| {
        anyhow::anyhow!(
            "could not load the pdfium shared library ({e}). It is a runtime \
             dependency of the `pdf-render` feature, not a build-time one: \
             install pdfium-binaries and set PDFIUM_DYNAMIC_LIB_PATH to its lib \
             directory, or use the document's text layer instead."
        )
    })?;
    let pdfium = Pdfium::new(bindings);

    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| anyhow::anyhow!("opening {} for rendering: {e}", path.display()))?;

    let pages = document.pages();
    let count = pages.len();
    if page >= count as u32 {
        anyhow::bail!("page {page} is out of range; the document has {count}");
    }

    // pdfium counts pages and pixels in i32, so the conversions are its API's
    // rather than a choice made here. The page is bound to a local because
    // `render_with_config` borrows it — inlining the call would drop the page
    // while the render still refers to it.
    let target = pages
        .get(page as i32)
        .map_err(|e| anyhow::anyhow!("reading page {page}: {e}"))?;
    let rendered = target
        .render_with_config(&PdfRenderConfig::new().set_target_width(i32::from(width)))
        .map_err(|e| anyhow::anyhow!("rendering page {page}: {e}"))?;

    let mut png = std::io::Cursor::new(Vec::new());
    rendered
        .as_image()
        .map_err(|e| anyhow::anyhow!("converting page {page} to an image: {e}"))?
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("encoding page {page} as PNG: {e}"))?;

    Ok(png.into_inner())
}

/// Render one page to a PNG, zero-based.
///
/// This build has no PDF renderer; see the `pdf-render` feature.
#[cfg(not(feature = "pdf-render"))]
pub fn render_page(_path: &Path, _page: u32, _width: u16) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!(
        "this build cannot render PDF pages to images: rebuild with the \
         `pdf-render` feature, or use the document's text layer instead"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_the_extractor_never_split_is_all_of_segment_zero() {
        // The shape of every PDF in the CMS 1115 corpus: real text, no form
        // feeds. Page zero must be the whole thing rather than nothing.
        let text = "227 pages of policy with no page breaks at all";
        assert_eq!(text_segments(text), 1);
        assert_eq!(page_text_from(text, 0, 227).unwrap(), text);
    }

    #[test]
    fn asking_past_the_last_segment_names_both_counts_and_neither_blames_a_scan() {
        // The message this replaces said the page "may be scanned" and to
        // check `has_text_layer` — which was `true`, with the text sitting
        // in segment zero. A real user followed that advice toward replacing
        // the extractor entirely.
        let err = page_text_from("one segment only", 1, 227)
            .expect_err("page 1 of a single-segment document must fail");
        let message = err.to_string();

        assert!(message.contains("1 addressable text segment"), "{message}");
        assert!(message.contains("227 page(s)"), "{message}");
        assert!(
            message.contains("document_text"),
            "the remedy must be the one that works: {message}"
        );
        assert!(
            !message.contains("has_text_layer"),
            "must not send the reader back to a flag that is already true: {message}"
        );
    }

    #[test]
    fn a_document_with_real_page_breaks_still_indexes_by_page() {
        // The case the old code got right, which must keep working.
        let text = "first\u{c}second\u{c}third";
        assert_eq!(text_segments(text), 3);
        assert_eq!(page_text_from(text, 0, 3).unwrap(), "first");
        assert_eq!(page_text_from(text, 2, 3).unwrap(), "third");
        assert!(page_text_from(text, 3, 3).is_err());
    }
}
