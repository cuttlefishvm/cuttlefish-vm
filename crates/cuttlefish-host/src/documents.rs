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

/// Extract one page's text, zero-based.
pub fn page_text(path: &Path, page: u32) -> anyhow::Result<String> {
    // pdf_extract works a whole document at a time, so this extracts everything
    // and takes the page wanted. Wasteful for a large document read page by
    // page, and worth replacing when that becomes a real workload rather than a
    // hypothetical one.
    let doc = lopdf::Document::load(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let count = doc.get_pages().len() as u32;
    if page >= count {
        anyhow::bail!("page {page} is out of range; the document has {count}");
    }

    let text = pdf_extract::extract_text(path)
        .map_err(|e| anyhow::anyhow!("extracting text from {}: {e}", path.display()))?;

    // Page breaks are form feeds in pdf_extract's output. When they are absent —
    // a single-page document, or one it did not mark — the whole text is the
    // only sensible answer for page zero.
    let pages: Vec<&str> = text.split('\u{c}').collect();
    Ok(pages
        .get(page as usize)
        .map(|s| s.to_string())
        .unwrap_or_else(|| if page == 0 { text } else { String::new() }))
}

/// Render one page to a PNG, zero-based.
///
/// Requires the `pdf-render` feature.
#[cfg(feature = "pdf-render")]
pub fn render_page(path: &Path, page: u32, width: u16) -> anyhow::Result<Vec<u8>> {
    use pdfium_render::prelude::*;

    let pdfium = Pdfium::default();
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| anyhow::anyhow!("opening {} for rendering: {e}", path.display()))?;

    let pages = document.pages();
    let count = pages.len();
    if page >= count as u32 {
        anyhow::bail!("page {page} is out of range; the document has {count}");
    }

    let rendered = pages
        .get(page as u16)
        .map_err(|e| anyhow::anyhow!("reading page {page}: {e}"))?
        .render_with_config(&PdfRenderConfig::new().set_target_width(width))
        .map_err(|e| anyhow::anyhow!("rendering page {page}: {e}"))?;

    let mut png = std::io::Cursor::new(Vec::new());
    rendered
        .as_image()
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
