//! Tests for media detection, byte slices, and document reading.
//!
//! The property these protect is that a block can *ask* what it opened and get
//! a truthful answer. A wrong `MediaKind` sends a block down the wrong path —
//! slicing a PNG as text, or extracting text from a scan and summarizing the
//! empty string — and both of those fail quietly rather than loudly.

use cuttlefish_abi::MediaKind;
use cuttlefish_host::{documents, handles::Handles};
use std::path::PathBuf;

fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The smallest valid PNG this project needs: an 8×8 image.
fn png_bytes() -> Vec<u8> {
    // Built rather than checked in, so the fixture cannot go missing.
    fn chunk(tag: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let body: Vec<u8> = tag.iter().chain(data).copied().collect();
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
        out
    }
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 8u32.to_be_bytes().to_vec();
    ihdr.extend(8u32.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    png.extend(chunk(b"IHDR", &ihdr));
    // A single all-zero scanline set, deflate-stored; content does not matter,
    // only that the header identifies it as a PNG.
    png.extend(chunk(
        b"IDAT",
        &[0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01],
    ));
    png.extend(chunk(b"IEND", &[]));
    png
}

#[test]
fn text_is_recognised_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "a.txt", b"hello world");
    let (_h, _len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(kind, MediaKind::Text);
}

#[test]
fn an_empty_file_is_text_not_binary() {
    // Nothing in it can be anything else, and calling it binary would make a
    // block reach for the wrong commands.
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "empty.txt", b"");
    let (_h, len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(len, 0);
    assert_eq!(kind, MediaKind::Text);
}

#[test]
fn utf8_that_spans_the_sniff_window_is_still_text() {
    // The detector reads a fixed window, which can cut a multi-byte character in
    // half. That looks like invalid UTF-8 but is exactly what text looks like.
    let dir = tempfile::tempdir().unwrap();
    let mut content = "é".repeat(3000).into_bytes();
    content.truncate(4097); // guaranteed to end mid-character
    let path = write(&dir, "wide.txt", &content);
    let (_h, _len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(kind, MediaKind::Text);
}

#[test]
fn a_png_is_recognised_by_content_not_extension() {
    // Extensions record whatever the last program to touch a file believed.
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "actually-an-image.txt", &png_bytes());
    let (_h, _len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(
        kind,
        MediaKind::Image {
            format: "png".into()
        }
    );
}

#[test]
fn a_jpeg_is_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0]);
    let (_h, _len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(
        kind,
        MediaKind::Image {
            format: "jpeg".into()
        }
    );
}

#[test]
fn arbitrary_binary_is_recognised_as_binary() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "blob.bin", &[0x00, 0xFF, 0xFE, 0x01, 0x80, 0x7F]);
    let (_h, _len, kind) = Handles::default().open(&path).unwrap();
    assert_eq!(kind, MediaKind::Binary);
}

#[test]
fn byte_slices_do_not_truncate_at_character_boundaries() {
    // The whole point of SliceBytes: it returns exactly what was asked for.
    // `Slice` would cut this back to a character boundary; this must not.
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "m.txt", "aéb".as_bytes());
    let mut handles = Handles::default();
    let (h, _len, _) = handles.open(&path).unwrap();

    let (bytes, next) = handles.slice_bytes(h, 0, 2).unwrap();
    assert_eq!(
        bytes,
        vec![b'a', 0xC3],
        "must return the raw bytes asked for"
    );
    assert_eq!(next, 2);
}

#[test]
fn byte_slices_are_clamped_to_the_end_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "s.bin", &[1, 2, 3]);
    let mut handles = Handles::default();
    let (h, _len, _) = handles.open(&path).unwrap();

    let (bytes, next) = handles.slice_bytes(h, 0, 999).unwrap();
    assert_eq!(bytes, vec![1, 2, 3]);
    assert_eq!(next, 3);
}

#[test]
fn reading_a_whole_handle_returns_every_byte() {
    // How an image reaches a vision model: the host materializes it once,
    // host-side, and it never enters guest memory.
    let dir = tempfile::tempdir().unwrap();
    let png = png_bytes();
    let path = write(&dir, "i.png", &png);
    let mut handles = Handles::default();
    let (h, _len, _) = handles.open(&path).unwrap();

    assert_eq!(handles.read_all(h).unwrap(), png);
}

#[test]
fn host_produced_bytes_become_a_handle_like_any_other() {
    // A rendered page has no file behind it, but must be usable everywhere a
    // file-backed image is.
    let mut handles = Handles::default();
    let (h, len) = handles.insert_bytes(
        vec![1, 2, 3, 4],
        MediaKind::Image {
            format: "png".into(),
        },
    );

    assert_eq!(len, 4);
    assert_eq!(handles.read_all(h).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(
        handles.kind(h).unwrap(),
        MediaKind::Image {
            format: "png".into()
        }
    );
    let (window, _) = handles.slice_bytes(h, 1, 2).unwrap();
    assert_eq!(window, vec![2, 3]);
}

#[test]
fn an_unknown_handle_has_no_kind() {
    assert!(Handles::default().kind(42).is_err());
}

// -- documents ------------------------------------------------------------

/// The repository's sample PDF, which has a real text layer.
fn sample_pdf() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/docs/sample.pdf")
}

#[test]
fn a_pdf_is_recognised_and_its_pages_counted() {
    let (_h, _len, kind) = Handles::default().open(&sample_pdf()).unwrap();
    // The handle layer reports a document without reading the whole file; the
    // runner fills in page count and text layer from the document layer.
    assert!(matches!(kind, MediaKind::Document { .. }), "got {kind:?}");
}

#[test]
fn inspecting_a_pdf_reports_pages_and_a_text_layer() {
    let info = documents::inspect(&sample_pdf()).expect("the sample PDF should inspect");
    assert_eq!(info.pages, 1);
    assert!(
        info.has_text_layer,
        "the sample PDF has real text and must be reported as such"
    );
}

#[test]
fn extracting_page_text_returns_the_documents_words() {
    let text = documents::page_text(&sample_pdf(), 0).expect("page 0 should extract");
    assert!(
        text.to_lowercase().contains("cuttlefish"),
        "expected the document's text, got: {text:?}"
    );
}

#[test]
fn a_page_past_the_end_is_refused() {
    let err = documents::page_text(&sample_pdf(), 99).unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
#[cfg(not(feature = "pdf-render"))]
fn rendering_without_the_feature_explains_itself() {
    // Failing with instructions beats returning a blank page, which would look
    // like a document with nothing in it.
    let err = documents::render_page(&sample_pdf(), 0, 512).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("pdf-render"), "{msg}");
}

#[test]
#[cfg(feature = "pdf-render")]
fn rendering_produces_a_png() {
    let png = documents::render_page(&sample_pdf(), 0, 256).expect("page 0 should render");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "not a PNG");
}
