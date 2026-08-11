//! Binary inspection for scripts: identify, measure, and parse bytes.
//!
//! Everything here is a *pure function over bytes*. A script gets bytes from
//! `slice_bytes(handle, off, len)`, which returns `{bytes_base64, ...}`, and
//! passes that string straight in. Nothing in this module talks to the host,
//! which is what keeps it out of the suspend/replay machinery entirely: a
//! pure call has no round trip to memoize, so it needs no call index, no
//! pending command, and no log entry.
//!
//! # Why parsing and not decoding
//!
//! Reading a container's structure is small — an MP4 atom is a length and a
//! four-byte tag, a tar header is 512 fixed bytes, a ZIP central directory is
//! a table. Decoding the *content* is what costs megabytes (video codecs,
//! image formats), and that belongs on the host behind a feature flag, the
//! way `pdf-render` already is. This module deliberately stops at structure,
//! which is where most of the analytical value sits anyway: "what is this,
//! what's inside it, does it look packed" rarely requires decoding a single
//! frame.
//!
//! The one exception is DEFLATE, which archives need to be useful at all and
//! which is small enough to carry.
//!
//! # Hostile input is the normal case
//!
//! These parsers exist to look at files nobody vouched for. A length field
//! that claims four gigabytes, an atom nested inside itself, a truncated
//! header — all are ordinary here, not edge cases. Every parser is written
//! to return an error rather than panic, because a panic in a guest is a
//! wasm trap, and a trap kills the whole fan-out item rather than recording
//! "this one file is malformed" and moving on.

use base64::Engine as _;
use serde_json::{json, Value};

/// Decode the base64 a script got from `slice_bytes`.
pub fn decode(bytes_base64: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(bytes_base64.trim())
        .map_err(|e| format!("not valid base64: {e}"))
}

/// Shannon entropy in bits per byte, 0.0 to 8.0.
///
/// The cheapest signal there is for "are these bytes compressed, encrypted,
/// or packed": English text sits near 4.5, a PNG or a zip near 7.99. It
/// cannot distinguish encryption from compression — both are near-maximal —
/// so it answers "is this opaque", not "is this a secret".
pub fn entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            p * p.log2()
        })
        .sum::<f64>()
}

/// How often each byte value occurs, as a 256-element array.
pub fn byte_histogram(bytes: &[u8]) -> Vec<u64> {
    let mut counts = vec![0u64; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    counts
}

/// Runs of printable ASCII at least `min_len` long, with their offsets.
///
/// The first thing anyone runs on an unknown binary. Printable is taken as
/// 0x20..=0x7e plus tab — deliberately not including newline, so a run
/// reported here is one line's worth of text rather than a whole file
/// collapsed into a single entry.
pub fn strings(bytes: &[u8], min_len: usize) -> Vec<Value> {
    let min_len = min_len.max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut run = 0usize;

    let flush = |start: usize, run: usize, out: &mut Vec<Value>| {
        if run >= min_len {
            let text = String::from_utf8_lossy(&bytes[start..start + run]).into_owned();
            out.push(json!({ "offset": start, "text": text }));
        }
    };

    for (i, &b) in bytes.iter().enumerate() {
        let printable = (0x20..=0x7e).contains(&b) || b == b'\t';
        if printable {
            if run == 0 {
                start = i;
            }
            run += 1;
        } else {
            flush(start, run, &mut out);
            run = 0;
        }
    }
    flush(start, run, &mut out);
    out
}

/// Classic hex+ASCII dump, 16 bytes per line.
///
/// Exists so a *model* can read the bytes. Handing a vision-less model raw
/// base64 tells it nothing; a hexdump is the representation everything in
/// its training data uses for "here are some bytes".
pub fn hexdump(bytes: &[u8], base_offset: u64) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let offset = base_offset + (row * 16) as u64;
        out.push_str(&format!("{offset:08x}  "));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            out.push(if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// A format guess from leading bytes.
///
/// Content, never the file name — the whole point is to be right about a
/// file called `data.txt` that is really a zip. `confidence` is coarse on
/// purpose: `high` for a magic number long and specific enough that a false
/// positive is implausible, `medium` for short or common signatures.
pub fn identify(bytes: &[u8]) -> Value {
    // Ordered longest/most-specific first, since some signatures are
    // prefixes of others and the first match wins.
    const SIGNATURES: &[(&[u8], &str, &str, &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "png", "image/png", "high"),
        (b"GIF89a", "gif", "image/gif", "high"),
        (b"GIF87a", "gif", "image/gif", "high"),
        (b"%PDF-", "pdf", "application/pdf", "high"),
        (b"\x7fELF", "elf", "application/x-executable", "high"),
        (b"PK\x03\x04", "zip", "application/zip", "high"),
        (b"PK\x05\x06", "zip", "application/zip", "high"),
        (b"\x1f\x8b", "gzip", "application/gzip", "high"),
        (b"BZh", "bzip2", "application/x-bzip2", "medium"),
        (b"\xfd7zXZ\x00", "xz", "application/x-xz", "high"),
        (b"\x28\xb5\x2f\xfd", "zstd", "application/zstd", "high"),
        (
            b"7z\xbc\xaf\x27\x1c",
            "7z",
            "application/x-7z-compressed",
            "high",
        ),
        (b"Rar!\x1a\x07", "rar", "application/vnd.rar", "high"),
        (b"OggS", "ogg", "application/ogg", "high"),
        (b"fLaC", "flac", "audio/flac", "high"),
        (b"\xff\xd8\xff", "jpeg", "image/jpeg", "high"),
        (
            b"MZ",
            "pe",
            "application/vnd.microsoft.portable-executable",
            "medium",
        ),
        (
            b"\x25\x21PS",
            "postscript",
            "application/postscript",
            "medium",
        ),
        (
            b"SQLite format 3\x00",
            "sqlite",
            "application/vnd.sqlite3",
            "high",
        ),
        (b"\x00asm", "wasm", "application/wasm", "high"),
    ];

    for (magic, format, mime, confidence) in SIGNATURES {
        if bytes.starts_with(magic) {
            return json!({ "format": format, "mime": mime, "confidence": confidence });
        }
    }

    // Signatures that aren't at offset 0.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        // The brand distinguishes MP4 from HEIF and friends; report it
        // rather than flattening every ISO-BMFF file to "mp4".
        let brand = String::from_utf8_lossy(&bytes[8..12]).trim().to_string();
        let (format, mime) = match &brand[..brand.len().min(3)] {
            "hei" | "mif" => ("heif", "image/heif"),
            "qt " => ("mov", "video/quicktime"),
            _ => ("mp4", "video/mp4"),
        };
        return json!({ "format": format, "mime": mime, "confidence": "high", "brand": brand });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") {
        let form = String::from_utf8_lossy(&bytes[8..12]).to_string();
        let (format, mime) = match form.as_str() {
            "WAVE" => ("wav", "audio/wav"),
            "AVI " => ("avi", "video/x-msvideo"),
            "WEBP" => ("webp", "image/webp"),
            _ => ("riff", "application/octet-stream"),
        };
        return json!({ "format": format, "mime": mime, "confidence": "high" });
    }
    // A tar has no leading magic at all — its signature sits at offset 257,
    // which is why a tarball is so often misidentified as "data".
    if bytes.len() >= 262 && (&bytes[257..262] == b"ustar") {
        return json!({ "format": "tar", "mime": "application/x-tar", "confidence": "high" });
    }

    if std::str::from_utf8(bytes).is_ok() {
        return json!({ "format": "text", "mime": "text/plain", "confidence": "medium" });
    }
    json!({ "format": "unknown", "mime": "application/octet-stream", "confidence": "low" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_separates_uniform_from_random_looking_bytes() {
        // All one byte: no information at all.
        assert_eq!(entropy(&[0u8; 1024]), 0.0);
        // Every byte value once: maximal.
        let all: Vec<u8> = (0..=255).collect();
        assert!((entropy(&all) - 8.0).abs() < 1e-9, "{}", entropy(&all));
        // Empty is defined, not a divide-by-zero.
        assert_eq!(entropy(&[]), 0.0);
        // Ordinary English sits well below the opaque threshold, which is
        // the discrimination the function exists to provide.
        assert!(entropy(b"the quick brown fox jumps over the lazy dog") < 5.0);
    }

    #[test]
    fn strings_finds_runs_and_respects_the_minimum() {
        let data = b"\x00\x01hello\x00ab\x00world!\xff";
        let found = strings(data, 5);
        let texts: Vec<String> = found
            .iter()
            .map(|v| v["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            texts,
            vec!["hello", "world!"],
            "short run `ab` must be dropped"
        );
        // Offsets must be real, or a caller can't seek back to what it found.
        assert_eq!(found[0]["offset"], 2);
    }

    #[test]
    fn strings_terminates_a_run_at_the_end_of_input() {
        // The off-by-one that a loop-only implementation misses: a run
        // touching the last byte is never flushed unless the end is handled.
        let found = strings(b"\x00trailing", 4);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["text"], "trailing");
    }

    #[test]
    fn identify_reads_content_not_names() {
        assert_eq!(identify(b"\x89PNG\r\n\x1a\n\x00\x00")["format"], "png");
        assert_eq!(identify(b"PK\x03\x04rest")["format"], "zip");
        assert_eq!(identify(b"\x00asm\x01\x00\x00\x00")["format"], "wasm");
        assert_eq!(identify(b"plain words here")["format"], "text");
        assert_eq!(identify(&[0x00, 0xff, 0xfe, 0x01])["format"], "unknown");
    }

    #[test]
    fn identify_finds_signatures_that_are_not_at_offset_zero() {
        // A tar's `ustar` marker lives at 257, which is why tarballs so
        // often get reported as featureless data.
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(identify(&tar)["format"], "tar");

        // ISO-BMFF: the brand at offset 8 separates mp4 from heif/mov.
        let mut mp4 = vec![0u8; 16];
        mp4[4..8].copy_from_slice(b"ftyp");
        mp4[8..12].copy_from_slice(b"isom");
        assert_eq!(identify(&mp4)["format"], "mp4");
        mp4[8..12].copy_from_slice(b"heic");
        assert_eq!(identify(&mp4)["format"], "heif");

        let mut wav = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        wav.extend_from_slice(&[0u8; 4]);
        assert_eq!(identify(&wav)["format"], "wav");
    }

    #[test]
    fn identify_never_panics_on_short_or_hostile_input() {
        // Every length from empty to past the tar marker: the parsers index
        // at fixed offsets, and a guest panic is a wasm trap that kills the
        // whole fan-out item rather than failing one file.
        for len in 0..300 {
            let _ = identify(&vec![0xffu8; len]);
            let _ = identify(&vec![0x00u8; len]);
        }
    }

    #[test]
    fn hexdump_pads_a_short_final_row_and_stays_aligned() {
        let dump = hexdump(b"AB", 0);
        assert!(dump.starts_with("00000000  41 42 "), "{dump}");
        assert!(dump.trim_end().ends_with("|AB|"), "{dump}");
        // Unprintables must render as dots, not as raw control characters
        // that would corrupt the surrounding output.
        assert!(hexdump(&[0x00, 0x07], 0).trim_end().ends_with("|..|"));
    }

    #[test]
    fn hexdump_offsets_are_absolute_not_relative() {
        // A caller dumping a window mid-file needs offsets it can seek to.
        let dump = hexdump(&[0u8; 17], 0x1000);
        assert!(dump.starts_with("00001000  "), "{dump}");
        assert!(dump.contains("\n00001010  "), "{dump}");
    }

    #[test]
    fn decode_rejects_garbage_rather_than_silently_returning_nothing() {
        assert!(decode("!!!!").is_err());
        assert_eq!(decode("aGk=").unwrap(), b"hi");
        // Scripts concatenate and indent; tolerate surrounding whitespace.
        assert_eq!(decode("  aGk=\n").unwrap(), b"hi");
    }
}
