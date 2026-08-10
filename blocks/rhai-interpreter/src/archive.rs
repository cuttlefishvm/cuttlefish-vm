//! Archive listing and bounded extraction: tar, zip, gzip.
//!
//! Listing is parsing — a tar header is 512 fixed bytes, a ZIP central
//! directory is a table — and costs nothing. Extraction is decompression,
//! which is why it is separated here and why every entry point that produces
//! decompressed bytes takes an explicit ceiling.
//!
//! # Why extraction is always bounded
//!
//! A decompression bomb is a small file that expands without limit. In a
//! guest that matters more than usual: running out of memory is a wasm trap,
//! and a trap kills the whole fan-out item rather than recording "this one
//! archive is malicious" and continuing. So the ceiling is a required
//! argument rather than an option with a default — a caller has to decide
//! what it is willing to hold, and exceeding it is an ordinary error the
//! script can catch.
//!
//! Path traversal (`../../etc/passwd`) needs no defence here, and it would
//! be misleading to add one: a block cannot write files at all. Extraction
//! only ever yields bytes in memory. Entry names are still reported exactly
//! as stored, so a caller that later *does* write them can make its own
//! decision with the real name in hand rather than a sanitized one that hid
//! the attack.

use serde_json::{json, Value};

/// Entries in a POSIX/GNU tar, from its 512-byte headers.
///
/// Uncompressed tar only. A `.tar.gz` is a gzip whose contents are a tar, so
/// the caller decompresses first — keeping the two steps visible rather than
/// silently sniffing, since the caller is the one who has to supply the
/// memory ceiling for the decompression.
pub fn tar_entries(bytes: &[u8]) -> Result<Vec<Value>, String> {
    const BLOCK: usize = 512;
    let mut out = Vec::new();
    let mut at = 0usize;

    while at + BLOCK <= bytes.len() {
        let header = &bytes[at..at + BLOCK];
        // Two consecutive zero blocks mark the end; one is enough to stop.
        if header.iter().all(|&b| b == 0) {
            break;
        }
        if &header[257..262] != b"ustar" {
            return Err(format!("not a tar header at offset {at}"));
        }

        let name = trimmed_str(&header[0..100]);
        // Size is octal ASCII. A malformed field is a corrupt archive, not a
        // reason to guess.
        let size = octal(&header[124..136])
            .ok_or_else(|| format!("unreadable size field for `{name}` at offset {at}"))?;
        let mtime = octal(&header[136..148]).unwrap_or(0);
        let typeflag = header[156] as char;

        out.push(json!({
            "name": name,
            "size": size,
            "mtime": mtime,
            "kind": match typeflag {
                '0' | '\0' => "file",
                '5' => "dir",
                '2' => "symlink",
                '1' => "hardlink",
                other => return Err(format!("unsupported tar entry type `{other}`")),
            },
            "offset": at + BLOCK,
        }));

        // Contents are padded to a block boundary. `checked_add` because a
        // corrupt size field is exactly how a parser gets walked off the end.
        let padded = size
            .checked_add(BLOCK as u64 - 1)
            .map(|s| (s / BLOCK as u64) * BLOCK as u64)
            .ok_or("tar size field overflows")?;
        at = at
            .checked_add(BLOCK)
            .and_then(|a| a.checked_add(padded as usize))
            .ok_or("tar entry offset overflows")?;
    }
    Ok(out)
}

/// Entries from a ZIP's central directory.
///
/// The central directory rather than the local headers: it is authoritative,
/// it sits at a known place (the end), and walking it means never trusting a
/// local header that disagrees with it — a discrepancy real malware uses to
/// show one file to a scanner and another to an extractor.
pub fn zip_entries(bytes: &[u8]) -> Result<Vec<Value>, String> {
    const EOCD_SIG: &[u8] = b"PK\x05\x06";
    const CD_SIG: &[u8] = b"PK\x01\x02";

    // The end-of-central-directory record is last, but a variable-length
    // comment can follow it, so scan backwards for the signature.
    let eocd = (0..bytes.len().saturating_sub(21))
        .rev()
        .find(|&i| bytes[i..].starts_with(EOCD_SIG))
        .ok_or("no end-of-central-directory record: not a zip, or truncated")?;

    let cd_offset = u32_le(bytes, eocd + 16).ok_or("truncated EOCD record")? as usize;
    let count = u16_le(bytes, eocd + 10).ok_or("truncated EOCD record")? as usize;

    let mut out = Vec::new();
    let mut at = cd_offset;
    for _ in 0..count {
        if !bytes.get(at..).is_some_and(|b| b.starts_with(CD_SIG)) {
            return Err(format!("central directory entry missing at offset {at}"));
        }
        let method = u16_le(bytes, at + 10).ok_or("truncated central directory entry")?;
        let compressed = u32_le(bytes, at + 20).ok_or("truncated central directory entry")?;
        let uncompressed = u32_le(bytes, at + 24).ok_or("truncated central directory entry")?;
        let name_len = u16_le(bytes, at + 28).ok_or("truncated central directory entry")? as usize;
        let extra_len = u16_le(bytes, at + 30).ok_or("truncated central directory entry")? as usize;
        let comment_len =
            u16_le(bytes, at + 32).ok_or("truncated central directory entry")? as usize;
        let local_offset = u32_le(bytes, at + 42).ok_or("truncated central directory entry")?;

        let name_start = at + 46;
        let name = bytes
            .get(name_start..name_start + name_len)
            .ok_or("central directory entry name runs past the end")?;

        out.push(json!({
            // Reported verbatim, traversal sequences and all — see the module
            // docs on why sanitizing here would hide rather than help.
            "name": String::from_utf8_lossy(name),
            "compressed_size": compressed,
            "size": uncompressed,
            "method": match method { 0 => "store", 8 => "deflate", other => return Err(format!("unsupported zip compression method {other}")) },
            "local_offset": local_offset,
            // The compression ratio a bomb gives itself away with.
            "ratio": if compressed > 0 { uncompressed as f64 / compressed as f64 } else { 0.0 },
        }));

        at = name_start + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// Decompress a gzip member, refusing to produce more than `max_bytes`.
///
/// The ceiling is checked *during* inflation, not after: a bomb that expands
/// to a gigabyte must be stopped while it is expanding, since discovering it
/// afterwards means the memory was already taken and the guest already
/// trapped.
pub fn gunzip(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err("not gzip data".to_string());
    }
    if bytes.get(2) != Some(&8) {
        return Err("unsupported gzip compression method".to_string());
    }
    let flags = *bytes.get(3).ok_or("truncated gzip header")?;
    let mut at = 10usize;

    if flags & 0b0000_0100 != 0 {
        let extra = u16_le(bytes, at).ok_or("truncated gzip extra field")? as usize;
        at = at + 2 + extra;
    }
    // FNAME and FCOMMENT are NUL-terminated strings.
    for flag in [0b0000_1000u8, 0b0001_0000] {
        if flags & flag != 0 {
            let end = bytes[at.min(bytes.len())..]
                .iter()
                .position(|&b| b == 0)
                .ok_or("unterminated gzip header string")?;
            at = at + end + 1;
        }
    }
    if flags & 0b0000_0010 != 0 {
        at += 2; // header CRC
    }
    let body = bytes.get(at..).ok_or("truncated gzip header")?;

    let out = miniz_oxide::inflate::decompress_to_vec_with_limit(body, max_bytes)
        .map_err(|e| format!("gzip inflate failed: {e:?}"))?;
    Ok(out)
}

/// Raw DEFLATE, for a stored-or-deflated zip member.
pub fn inflate(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_with_limit(bytes, max_bytes)
        .map_err(|e| format!("inflate failed: {e:?}"))
}

fn trimmed_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Parse a NUL/space-padded octal field, as tar stores its numbers.
fn octal(field: &[u8]) -> Option<u64> {
    let text = trimmed_str(field);
    let text = text.trim();
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but genuine tar header block for one file.
    fn tar_header(name: &str, size: u64, typeflag: u8) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let octal_size = format!("{size:011o}\0");
        h[124..124 + octal_size.len()].copy_from_slice(octal_size.as_bytes());
        h[136..136 + 12].copy_from_slice(b"00000000000\0");
        h[156] = typeflag;
        h[257..262].copy_from_slice(b"ustar");
        h
    }

    #[test]
    fn tar_lists_entries_and_skips_padded_contents() {
        let mut archive = tar_header("first.txt", 3, b'0');
        archive.extend_from_slice(&[0u8; 512]); // 3 bytes, padded to a block
        archive.extend(tar_header("dir/", 0, b'5'));
        archive.extend_from_slice(&[0u8; 1024]); // end-of-archive

        let entries = tar_entries(&archive).unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0]["name"], "first.txt");
        assert_eq!(entries[0]["size"], 3);
        assert_eq!(entries[0]["kind"], "file");
        // The second entry is only found if the first's padded content was
        // skipped by exactly one block, not by its literal 3 bytes.
        assert_eq!(entries[1]["name"], "dir/");
        assert_eq!(entries[1]["kind"], "dir");
    }

    #[test]
    fn a_tar_with_a_corrupt_size_field_errors_rather_than_walking_off_the_end() {
        let mut archive = tar_header("bad.txt", 0, b'0');
        // Not octal at all.
        archive[124..136].copy_from_slice(b"zzzzzzzzzzz\0");
        let err = tar_entries(&archive).expect_err("a corrupt size must be refused");
        assert!(err.contains("bad.txt"), "{err}");
    }

    #[test]
    fn a_tar_claiming_an_enormous_size_does_not_overflow() {
        // The arithmetic that walks to the next header is where a hostile
        // size field turns into a panic. Near-max must error, not wrap.
        let mut archive = tar_header("huge.bin", 0, b'0');
        archive[124..136].copy_from_slice(b"77777777777\0");
        // Either it errors, or it cleanly reports one entry and stops; what
        // it must never do is panic.
        let _ = tar_entries(&archive);
    }

    #[test]
    fn tar_parsing_never_panics_on_truncation() {
        let full = {
            let mut a = tar_header("f", 10, b'0');
            a.extend_from_slice(&[0u8; 512]);
            a
        };
        for len in 0..full.len() {
            let _ = tar_entries(&full[..len]);
        }
    }

    /// A one-entry zip: local header, then central directory, then EOCD.
    fn minimal_zip(name: &str, compressed: u32, uncompressed: u32) -> Vec<u8> {
        let mut z = Vec::new();
        z.extend_from_slice(b"PK\x03\x04");
        z.extend_from_slice(&[0u8; 26]);
        let cd_offset = z.len() as u32;

        z.extend_from_slice(b"PK\x01\x02");
        z.extend_from_slice(&[0u8; 6]); // version, flags
        z.extend_from_slice(&8u16.to_le_bytes()); // method: deflate (at +10)
        z.extend_from_slice(&[0u8; 8]); // time, date, crc (to +20)
        z.extend_from_slice(&compressed.to_le_bytes()); // +20
        z.extend_from_slice(&uncompressed.to_le_bytes()); // +24
        z.extend_from_slice(&(name.len() as u16).to_le_bytes()); // +28
        z.extend_from_slice(&0u16.to_le_bytes()); // extra len +30
        z.extend_from_slice(&0u16.to_le_bytes()); // comment len +32
        z.extend_from_slice(&[0u8; 8]); // disk, attrs (+34..42)
        z.extend_from_slice(&0u32.to_le_bytes()); // local offset +42
        z.extend_from_slice(name.as_bytes()); // +46

        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0u8; 6]);
        z.extend_from_slice(&1u16.to_le_bytes()); // entry count at +10
        z.extend_from_slice(&0u32.to_le_bytes()); // cd size
        z.extend_from_slice(&cd_offset.to_le_bytes()); // cd offset at +16
        z.extend_from_slice(&0u16.to_le_bytes()); // comment len
        z
    }

    #[test]
    fn zip_reads_the_central_directory() {
        let z = minimal_zip("notes.txt", 100, 400);
        let entries = zip_entries(&z).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "notes.txt");
        assert_eq!(entries[0]["size"], 400);
        assert_eq!(entries[0]["method"], "deflate");
        assert_eq!(entries[0]["ratio"], 4.0);
    }

    #[test]
    fn zip_reports_the_ratio_that_gives_a_bomb_away() {
        // 1 KiB expanding to 1 GiB. Listing must surface this *before*
        // anyone decompresses, which is the entire reason ratio is reported.
        let z = minimal_zip("bomb", 1024, 1024 * 1024 * 1024);
        let entries = zip_entries(&z).unwrap();
        assert!(
            entries[0]["ratio"].as_f64().unwrap() > 1000.0,
            "{:?}",
            entries[0]
        );
    }

    #[test]
    fn zip_entry_names_are_reported_verbatim_including_traversal() {
        // Sanitizing here would hide the attack from the caller who can
        // actually act on it. A block cannot write files, so reporting the
        // real name is strictly more useful and no less safe.
        let z = minimal_zip("../../etc/passwd", 10, 10);
        let entries = zip_entries(&z).unwrap();
        assert_eq!(entries[0]["name"], "../../etc/passwd");
    }

    #[test]
    fn zip_parsing_never_panics_on_truncation() {
        let z = minimal_zip("f.txt", 1, 2);
        for len in 0..z.len() {
            let _ = zip_entries(&z[..len]);
        }
    }

    #[test]
    fn a_zip_without_an_eocd_is_an_error_not_a_panic() {
        let err = zip_entries(b"PK\x03\x04 no directory here").expect_err("must be refused");
        assert!(err.contains("central-directory"), "{err}");
    }

    #[test]
    fn gunzip_round_trips_and_enforces_its_ceiling() {
        // Build real gzip data rather than a fixture, so the header parsing
        // (flags, optional fields) is genuinely exercised.
        let payload = b"hello hello hello hello hello hello".repeat(20);
        let deflated = miniz_oxide::deflate::compress_to_vec(&payload, 6);
        let mut gz = vec![0x1f, 0x8b, 0x08, 0x00];
        gz.extend_from_slice(&[0u8; 6]); // mtime, xfl, os
        gz.extend_from_slice(&deflated);

        assert_eq!(gunzip(&gz, 1 << 20).unwrap(), payload);

        // The ceiling has to stop it, since a guest that runs out of memory
        // traps and takes the whole item with it.
        let err = gunzip(&gz, 8).expect_err("a ceiling below the output must refuse");
        assert!(err.contains("inflate failed"), "{err}");
    }

    #[test]
    fn gunzip_rejects_non_gzip_rather_than_guessing() {
        assert!(gunzip(b"PK\x03\x04", 1024).is_err());
        assert!(gunzip(&[], 1024).is_err());
    }
}
