//! Lowercase hex for digest bytes.
//!
//! Exists because `sha2` 0.11 (via `digest` 0.11 and `hybrid-array`) returns
//! a plain `Array` from `finalize`/`digest`, and that type no longer
//! implements `LowerHex` the way `GenericArray` did in 0.10. Every
//! `format!("{:x}", ...)` on a hash therefore stopped compiling.
//!
//! One helper rather than an inline `fold` at each of the four call sites:
//! these strings are content-addressing keys and graph fingerprints that get
//! compared against values already written to disk, so all four must agree
//! on the encoding exactly. A single function makes that agreement
//! structural instead of a thing four separate lines have to keep getting
//! right.
//!
//! Deliberately not a new dependency. The whole job is two lines, and a
//! digest is the only thing this codebase ever hex-encodes.

/// Encode `bytes` as lowercase hex, two characters per byte.
///
/// Byte-for-byte identical to what `format!("{:x}", digest)` produced under
/// `sha2` 0.10, which matters: existing blob filenames, catalog index
/// entries, and recorded graph fingerprints were all written with the old
/// formatting, and a change here would silently invalidate every one of
/// them.
pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    bytes.as_ref().iter().fold(String::new(), |mut out, b| {
        // Infallible: writing to a String never fails.
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn it_matches_the_formatting_sha2_0_10_produced() {
        // The well-known SHA-256 of the empty input, in the exact lowercase,
        // zero-padded, unseparated form the old `{:x}` produced. If this
        // ever drifts, every catalog blob name and graph fingerprint already
        // on disk stops matching.
        use sha2::{Digest, Sha256};
        assert_eq!(
            encode(Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_leading_zero_byte_keeps_both_of_its_characters() {
        // The failure mode a naive `{:x}` per byte would have: `0x0a`
        // rendering as "a" rather than "0a", which silently shortens the
        // string and collides distinct digests.
        assert_eq!(encode([0x00, 0x0a, 0xff]), "000aff");
    }
}
