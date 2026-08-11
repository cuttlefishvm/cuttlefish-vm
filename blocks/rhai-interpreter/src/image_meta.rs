//! Image metadata, parsed in the guest without decoding a single pixel.
//!
//! Dimensions, EXIF, PNG chunks, JPEG segments. All of it lives in headers,
//! so all of it is reachable by the same cheap byte parsing as
//! [`crate::binary`] — no codec, no host round trip, no feature flag.
//!
//! That boundary is deliberate and it is where this module stops. Reading
//! *what an image says about itself* is header parsing. Reading *what the
//! image looks like* requires decoding, which costs megabytes of codec and
//! belongs on the host behind a feature flag, the way `pdf-render` already
//! is. A surprising amount of real triage never crosses that line: when was
//! this taken, on what device, at what size, has it been re-encoded, does
//! the EXIF thumbnail still match the claimed content.
//!
//! Every parser here assumes hostile input, for the reasons in
//! [`crate::binary`]'s module docs: a panic in a guest is a wasm trap, and a
//! trap kills the whole fan-out item instead of failing one file.

use serde_json::{json, Value};

/// Pixel dimensions, read from whichever header the format keeps them in.
///
/// Cheap enough to run on every file in a corpus, and it is the single most
/// useful fact about an image after its format — a "photo" that is 1x1 is a
/// tracking pixel, and one whose dimensions disagree with its EXIF has been
/// re-encoded or tampered with.
pub fn dimensions(bytes: &[u8]) -> Result<Value, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // IHDR is required to be the first chunk: 8 magic + 4 length + 4 type.
        let w = be_u32(bytes, 16).ok_or("truncated PNG header")?;
        let h = be_u32(bytes, 20).ok_or("truncated PNG header")?;
        return Ok(json!({ "width": w, "height": h, "format": "png" }));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let w = le_u16(bytes, 6).ok_or("truncated GIF header")?;
        let h = le_u16(bytes, 8).ok_or("truncated GIF header")?;
        return Ok(json!({ "width": w, "height": h, "format": "gif" }));
    }
    if bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        // Only the simple lossy form keeps dimensions at a fixed offset;
        // say so rather than guessing for VP8L/VP8X.
        if bytes.len() > 30 && &bytes[12..16] == b"VP8 " {
            let w = le_u16(bytes, 26).ok_or("truncated WebP header")? & 0x3fff;
            let h = le_u16(bytes, 28).ok_or("truncated WebP header")? & 0x3fff;
            return Ok(json!({ "width": w, "height": h, "format": "webp" }));
        }
        return Err("WebP variant whose dimensions need a decoder".to_string());
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return jpeg_dimensions(bytes);
    }
    Err("dimensions: unrecognised image format".to_string())
}

/// Walk JPEG segments to the frame header that carries the size.
///
/// JPEG has no fixed offset for this: the SOFn marker sits after a variable
/// number of variable-length segments, so it must be walked. Which is also
/// why this is the parser most likely to meet a lying length field.
fn jpeg_dimensions(bytes: &[u8]) -> Result<Value, String> {
    for seg in jpeg_segments(bytes)? {
        let marker = seg["marker"].as_u64().unwrap_or(0) as u8;
        // SOF0..SOF15, excluding DHT (0xc4), JPG (0xc8) and DAC (0xcc),
        // which share the range but are not frame headers.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let at = seg["offset"].as_u64().unwrap_or(0) as usize;
            // offset points at the payload: precision(1), height(2), width(2)
            let h = be_u16(bytes, at + 1).ok_or("truncated JPEG frame header")?;
            let w = be_u16(bytes, at + 3).ok_or("truncated JPEG frame header")?;
            return Ok(json!({ "width": w, "height": h, "format": "jpeg" }));
        }
    }
    Err("no JPEG frame header found".to_string())
}

/// Every PNG chunk: type, length, offset.
///
/// Structure alone answers real questions — a `tEXt`/`iTXt` chunk carrying
/// a comment, an `eXIf` chunk, or trailing bytes after `IEND`, which is one
/// of the oldest ways to hide a payload inside an image that still renders.
pub fn png_chunks(bytes: &[u8]) -> Result<Vec<Value>, String> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("not a PNG".to_string());
    }
    let mut out = Vec::new();
    let mut at = 8usize;
    while at + 8 <= bytes.len() {
        let len = be_u32(bytes, at).ok_or("truncated PNG chunk header")? as usize;
        let kind = bytes
            .get(at + 4..at + 8)
            .ok_or("truncated PNG chunk type")?
            .to_vec();
        let name = String::from_utf8_lossy(&kind).to_string();
        out.push(json!({ "type": name, "length": len, "offset": at }));

        if name == "IEND" {
            // Anything past IEND is not part of the image. Report it: this
            // is where appended payloads hide.
            let end = at + 12;
            if bytes.len() > end {
                out.push(json!({
                    "type": "TRAILING",
                    "length": bytes.len() - end,
                    "offset": end,
                }));
            }
            break;
        }
        // length + type + data + crc, with checked arithmetic because the
        // length field is attacker-controlled.
        at = at
            .checked_add(12)
            .and_then(|a| a.checked_add(len))
            .ok_or("PNG chunk length overflows")?;
    }
    Ok(out)
}

/// Every JPEG segment: marker, length, payload offset.
pub fn jpeg_segments(bytes: &[u8]) -> Result<Vec<Value>, String> {
    if !bytes.starts_with(b"\xff\xd8") {
        return Err("not a JPEG".to_string());
    }
    let mut out = Vec::new();
    let mut at = 2usize;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            // Not at a marker boundary: entropy-coded data, or corruption.
            break;
        }
        let marker = bytes[at + 1];
        // Start-of-scan is followed by compressed data of unstated length;
        // walking further would be guessing.
        if marker == 0xda {
            out.push(json!({ "marker": marker, "name": "SOS", "offset": at + 2, "length": 0 }));
            break;
        }
        let len = be_u16(bytes, at + 2).ok_or("truncated JPEG segment length")? as usize;
        if len < 2 {
            return Err(format!("implausible JPEG segment length {len} at {at}"));
        }
        out.push(json!({
            "marker": marker,
            "name": jpeg_marker_name(marker),
            "offset": at + 4,
            "length": len - 2,
        }));
        at = at
            .checked_add(2)
            .and_then(|a| a.checked_add(len))
            .ok_or("JPEG segment length overflows")?;
    }
    Ok(out)
}

fn jpeg_marker_name(marker: u8) -> &'static str {
    match marker {
        0xc0 => "SOF0",
        0xc2 => "SOF2",
        0xc4 => "DHT",
        0xdb => "DQT",
        0xe0 => "APP0",
        0xe1 => "APP1",
        0xed => "APP13",
        0xee => "APP14",
        0xfe => "COM",
        _ => "other",
    }
}

/// EXIF tags from a JPEG's APP1 segment or a raw TIFF header.
///
/// Returns only tags worth a triage decision — camera, timestamps, GPS,
/// orientation, software — rather than the full tag space, because the
/// consumer is a model or a person deciding what to do next, and a hundred
/// obscure tags buries the five that matter.
pub fn exif(bytes: &[u8]) -> Result<Value, String> {
    let tiff = locate_tiff(bytes)?;
    let data = &bytes[tiff..];

    let little = match data.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err("EXIF has no byte-order mark".to_string()),
    };
    let ifd0 = read_u32(data, 4, little).ok_or("truncated EXIF header")? as usize;

    let mut found = serde_json::Map::new();
    // IFD0, then the Exif sub-IFD it points at, then GPS. Bounded rather
    // than followed recursively: a crafted file can point an IFD at itself.
    let mut queue = vec![ifd0];
    let mut seen = Vec::new();
    while let Some(offset) = queue.pop() {
        if seen.contains(&offset) || seen.len() > 8 {
            continue;
        }
        seen.push(offset);
        for (tag, value) in read_ifd(data, offset, little)? {
            match tag {
                // Pointers to further IFDs.
                0x8769 | 0x8825 => {
                    if let Some(next) = value.as_u64() {
                        queue.push(next as usize);
                    }
                }
                0x010f => insert(&mut found, "make", value),
                0x0110 => insert(&mut found, "model", value),
                0x0112 => insert(&mut found, "orientation", value),
                0x0131 => insert(&mut found, "software", value),
                0x0132 => insert(&mut found, "datetime", value),
                0x9003 => insert(&mut found, "datetime_original", value),
                0x829a => insert(&mut found, "exposure_time", value),
                0x8827 => insert(&mut found, "iso", value),
                0x0001 => insert(&mut found, "gps_lat_ref", value),
                0x0003 => insert(&mut found, "gps_lon_ref", value),
                _ => {}
            }
        }
    }
    Ok(Value::Object(found))
}

fn insert(map: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    map.entry(key.to_string()).or_insert(value);
}

/// Find the TIFF header EXIF is written inside, in a JPEG or a bare TIFF.
fn locate_tiff(bytes: &[u8]) -> Result<usize, String> {
    if bytes.starts_with(b"II*\x00") || bytes.starts_with(b"MM\x00*") {
        return Ok(0);
    }
    if bytes.starts_with(b"\xff\xd8") {
        for seg in jpeg_segments(bytes)? {
            if seg["marker"].as_u64() == Some(0xe1) {
                let at = seg["offset"].as_u64().unwrap_or(0) as usize;
                if bytes.get(at..at + 6) == Some(b"Exif\x00\x00") {
                    return Ok(at + 6);
                }
            }
        }
        return Err("JPEG has no EXIF segment".to_string());
    }
    Err("no EXIF: not a JPEG or TIFF".to_string())
}

/// Read one IFD's entries as `(tag, value)`, resolving short strings and
/// integers. Values needing more than four bytes are stored out-of-line.
fn read_ifd(data: &[u8], offset: usize, little: bool) -> Result<Vec<(u16, Value)>, String> {
    let count = read_u16(data, offset, little).ok_or("truncated IFD")? as usize;
    // A crafted count would otherwise drive a very long loop over garbage.
    if count > 512 {
        return Err(format!("implausible IFD entry count {count}"));
    }
    let mut out = Vec::new();
    for i in 0..count {
        let at = offset + 2 + i * 12;
        let Some(tag) = read_u16(data, at, little) else {
            break;
        };
        let format = read_u16(data, at + 2, little).unwrap_or(0);
        let components = read_u32(data, at + 4, little).unwrap_or(0) as usize;

        let value = match format {
            // ASCII
            2 => {
                let len = components.saturating_sub(1).min(4096);
                let start = if components <= 4 {
                    at + 8
                } else {
                    read_u32(data, at + 8, little).unwrap_or(0) as usize
                };
                match data.get(start..start + len) {
                    Some(s) => Value::String(String::from_utf8_lossy(s).trim().to_string()),
                    None => continue,
                }
            }
            // SHORT
            3 => match read_u16(data, at + 8, little) {
                Some(v) => json!(v),
                None => continue,
            },
            // LONG
            4 => match read_u32(data, at + 8, little) {
                Some(v) => json!(v),
                None => continue,
            },
            _ => continue,
        };
        out.push((tag, value));
    }
    Ok(out)
}

fn read_u16(d: &[u8], at: usize, little: bool) -> Option<u16> {
    let b: [u8; 2] = d.get(at..at + 2)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}

fn read_u32(d: &[u8], at: usize, little: bool) -> Option<u32> {
    let b: [u8; 4] = d.get(at..at + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

fn be_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}
fn le_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        p.extend_from_slice(&13u32.to_be_bytes());
        p.extend_from_slice(b"IHDR");
        p.extend_from_slice(&width.to_be_bytes());
        p.extend_from_slice(&height.to_be_bytes());
        p.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, colour type, etc.
        p.extend_from_slice(&[0u8; 4]); // crc
        p
    }

    #[test]
    fn dimensions_read_each_format_from_its_own_header() {
        let d = dimensions(&png(1920, 1080)).unwrap();
        assert_eq!(d["width"], 1920);
        assert_eq!(d["height"], 1080);

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&640u16.to_le_bytes());
        gif.extend_from_slice(&480u16.to_le_bytes());
        let d = dimensions(&gif).unwrap();
        assert_eq!(d["width"], 640);
        assert_eq!(d["height"], 480);
    }

    #[test]
    fn jpeg_dimensions_are_found_by_walking_to_the_frame_header() {
        // A JPEG keeps size in a SOFn segment at no fixed offset, so it has
        // to be walked past whatever segments precede it.
        let mut j = b"\xff\xd8".to_vec();
        j.extend_from_slice(&[0xff, 0xe0]); // APP0
        j.extend_from_slice(&8u16.to_be_bytes());
        j.extend_from_slice(&[0u8; 6]);
        j.extend_from_slice(&[0xff, 0xc0]); // SOF0
        j.extend_from_slice(&11u16.to_be_bytes());
        j.push(8); // precision
        j.extend_from_slice(&600u16.to_be_bytes()); // height
        j.extend_from_slice(&800u16.to_be_bytes()); // width
        j.extend_from_slice(&[0u8; 4]);

        let d = dimensions(&j).unwrap();
        assert_eq!(d["width"], 800);
        assert_eq!(d["height"], 600);
    }

    #[test]
    fn png_chunks_reports_bytes_appended_after_iend() {
        // One of the oldest ways to hide a payload in a file that still
        // renders as a normal image. Silence here would be the bug.
        let mut p = png(1, 1);
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"IEND");
        p.extend_from_slice(&[0u8; 4]);
        p.extend_from_slice(b"SECRET PAYLOAD");

        let chunks = png_chunks(&p).unwrap();
        let trailing = chunks
            .iter()
            .find(|c| c["type"] == "TRAILING")
            .expect("appended bytes must be reported");
        assert_eq!(trailing["length"], "SECRET PAYLOAD".len());
    }

    #[test]
    fn parsers_never_panic_on_truncation_or_hostile_lengths() {
        // A guest panic is a wasm trap that kills the whole fan-out item,
        // so malformed input must always come back as an error.
        let p = png(4, 4);
        for len in 0..p.len() {
            let _ = dimensions(&p[..len]);
            let _ = png_chunks(&p[..len]);
        }
        // A chunk length claiming most of the address space.
        let mut evil = png(1, 1);
        evil[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        let _ = png_chunks(&evil);

        let j = b"\xff\xd8\xff\xe1\x00\x01".to_vec(); // length < 2
        assert!(jpeg_segments(&j).is_err());

        for len in 0..64 {
            let _ = exif(&vec![0xffu8; len]);
            let _ = jpeg_segments(&vec![0xffu8; len]);
        }
    }

    #[test]
    fn exif_reads_camera_and_timestamp_from_a_real_tiff_header() {
        // Little-endian TIFF, one IFD, two ASCII tags stored out-of-line.
        let mut t = b"II*\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8
        t.extend_from_slice(&2u16.to_le_bytes()); // 2 entries

        let make = "Canon\0";
        let dt = "2026:08:09 12:00:00\0";
        let make_at = 8 + 2 + 24 + 4;
        let dt_at = make_at + make.len();

        for (tag, len, at) in [(0x010fu16, make.len(), make_at), (0x0132, dt.len(), dt_at)] {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&2u16.to_le_bytes()); // ASCII
            t.extend_from_slice(&(len as u32).to_le_bytes());
            t.extend_from_slice(&(at as u32).to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes()); // next IFD: none
        t.extend_from_slice(make.as_bytes());
        t.extend_from_slice(dt.as_bytes());

        let e = exif(&t).unwrap();
        assert_eq!(e["make"], "Canon");
        assert_eq!(e["datetime"], "2026:08:09 12:00:00");
    }

    #[test]
    fn exif_does_not_loop_forever_on_a_self_referential_ifd() {
        // A crafted file can point an IFD's sub-IFD pointer back at itself.
        // Unbounded following would hang the guest, which never times out
        // on its own.
        let mut t = b"II*\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x8769u16.to_le_bytes()); // Exif sub-IFD pointer
        t.extend_from_slice(&4u16.to_le_bytes()); // LONG
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&8u32.to_le_bytes()); // -> itself
        t.extend_from_slice(&0u32.to_le_bytes());

        // The assertion is simply that this returns.
        let _ = exif(&t).unwrap();
    }
}
