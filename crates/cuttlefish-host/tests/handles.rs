//! The handle table is what keeps bulk data out of guest memory: a block gets a
//! handle and a length, then pulls bounded windows.
//!
//! Most of these tests are about the UTF-8 boundary rule, because that is the
//! part a caller cannot see and will get wrong: window sizes are chosen with no
//! knowledge of where characters begin.

use cuttlefish_host::handles::Handles;

fn temp_with(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

#[test]
fn open_reports_length_without_reading_contents() {
    let (_d, path) = temp_with("hello world");
    let mut h = Handles::default();

    let (handle, len) = h.open(&path).unwrap();
    assert_eq!(len, 11);
    assert_eq!(handle, 0, "handles start at zero and are per-job");
}

#[test]
fn handles_are_distinct_per_open() {
    let (_d, path) = temp_with("x");
    let mut h = Handles::default();

    let (a, _) = h.open(&path).unwrap();
    let (b, _) = h.open(&path).unwrap();
    assert_ne!(a, b, "each open must get its own handle");
}

#[test]
fn slice_returns_the_requested_window() {
    let (_d, path) = temp_with("hello world");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    let w = h.slice(handle, 0, 5).unwrap();
    assert_eq!(w.text, "hello");
    assert_eq!(w.next_offset, 5);
}

#[test]
fn slice_reads_from_the_middle_of_a_file() {
    let (_d, path) = temp_with("hello world");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    let w = h.slice(handle, 6, 5).unwrap();
    assert_eq!(w.text, "world");
    assert_eq!(w.next_offset, 11);
}

#[test]
fn slice_is_clamped_to_the_end_of_the_file() {
    // Asking for more than remains is normal — a block does not know the file
    // length in advance of `Open`, and may ask for a full window at the tail.
    let (_d, path) = temp_with("hi");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    let w = h.slice(handle, 0, 4096).unwrap();
    assert_eq!(w.text, "hi");
    assert_eq!(w.next_offset, 2);
}

#[test]
fn slice_at_end_of_file_returns_empty_rather_than_erroring() {
    // A block looping until `next_offset == len` lands here exactly once.
    let (_d, path) = temp_with("hi");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    let w = h.slice(handle, 2, 10).unwrap();
    assert_eq!(w.text, "");
    assert_eq!(w.next_offset, 2);
}

#[test]
fn slice_truncates_to_a_character_boundary() {
    // "é" is two bytes, so a 2-byte window over "aéb" would split it.
    let (_d, path) = temp_with("aéb");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    let w = h.slice(handle, 0, 2).unwrap();
    assert_eq!(w.text, "a", "must not return half a character");
    assert_eq!(
        w.next_offset, 1,
        "next_offset must point at the start of the split character, not past it"
    );

    // Resuming from next_offset yields the character intact — this is the whole
    // point of reporting it.
    let w2 = h.slice(handle, w.next_offset, 2).unwrap();
    assert_eq!(w2.text, "é");
    assert_eq!(w2.next_offset, 3);
}

#[test]
fn walking_a_multibyte_file_in_tiny_windows_reconstructs_it_exactly() {
    // The property that actually matters: however the windows fall, a caller
    // that follows next_offset gets the original text back.
    let original = "héllo wörld — ünicode ✓ 日本語";
    let (_d, path) = temp_with(original);
    let mut h = Handles::default();
    let (handle, len) = h.open(&path).unwrap();

    let mut rebuilt = String::new();
    let mut offset = 0u64;
    while offset < len {
        let w = h.slice(handle, offset, 3).unwrap();
        assert!(w.next_offset > offset, "must always make progress");
        rebuilt.push_str(&w.text);
        offset = w.next_offset;
    }
    assert_eq!(rebuilt, original);
}

#[test]
fn a_window_too_small_for_one_character_is_an_error_not_an_empty_read() {
    // Returning empty here would be worse than failing: a caller looping until
    // it reaches the end would spin forever, making no progress and reporting
    // no problem.
    let (_d, path) = temp_with("é");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    assert!(h.slice(handle, 0, 1).is_err());
}

#[test]
fn an_unknown_handle_is_rejected() {
    let mut h = Handles::default();
    assert!(h.slice(99, 0, 10).is_err());
}

#[test]
fn an_offset_past_the_end_is_rejected() {
    let (_d, path) = temp_with("hi");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();

    assert!(h.slice(handle, 99, 1).is_err());
}

#[test]
fn opening_a_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Handles::default();

    assert!(h.open(&dir.path().join("nope.txt")).is_err());
}
