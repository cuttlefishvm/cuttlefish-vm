//! The local block catalog: maps `name@version` to a cataloged wasm block or
//! bundle, so a pipeline can reference a block by name instead of a
//! filesystem path.
//!
//! Storage is content-addressed and flat — no entry references another —
//! because a bundle's internal node structure lives entirely in its own
//! `.cfbundle` manifest, resolved to concrete blob hashes at build time; the
//! catalog index itself never needs to represent "this entry depends on that
//! entry."
//!
//! ```text
//! ~/.cuttlefish/catalog/
//!   blobs/<sha256>        raw wasm or bundle bytes, one copy per unique artifact
//!   index.json            name@version -> Entry
//!   index.json.lock       empty lock file guarding read-modify-write of index.json
//! ```
//!
//! See `docs/superpowers/specs/2026-08-02-block-catalog-design.md` for the
//! full design (that file is a gitignored working document, not committed;
//! this module doc is the durable record, per this project's
//! documentation-lives-in-the-code convention).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Current schema version of `index.json`'s own on-disk format — bumped only
/// when the *shape* of the index changes, never tied to this crate's version.
const INDEX_VERSION: u32 = 1;

/// Whether a cataloged artifact is a single wasm block or a multi-node bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A single compiled wasm module.
    Block,
    /// A `.cfbundle` container produced by `cuttlefish build`.
    Bundle,
}

/// One cataloged `name@version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Content hash of the artifact, as `sha256:<hex>`.
    pub hash: String,
    /// Block or bundle.
    pub kind: ArtifactKind,
    /// Compact `{input} -> {output}` signature string, cached at add-time so
    /// `list`/`show` never have to instantiate wasm just to answer "what does
    /// this accept and produce."
    pub signature: String,
    /// RFC 3339 timestamp, truncated to whole seconds, UTC. Used only to
    /// order "give me the latest" and to break did-you-mean ties — truncating
    /// to whole seconds keeps the string plain-comparable without parsing it
    /// back.
    pub created_at: String,
}

/// The whole on-disk `index.json`.
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

impl IndexFile {
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_file_serializes_to_the_shape_the_spec_documents() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "chunk-text@1".to_string(),
            Entry {
                hash: "sha256:9f86d081".to_string(),
                kind: ArtifactKind::Block,
                signature: "{path: text} -> [text]".to_string(),
                created_at: "2026-08-02T18:03:00Z".to_string(),
            },
        );
        let index = IndexFile {
            version: INDEX_VERSION,
            entries,
        };

        let json = serde_json::to_string(&index).expect("IndexFile always serializes");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("what we just wrote must parse");

        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["entries"]["chunk-text@1"]["kind"], "block");
        assert_eq!(
            parsed["entries"]["chunk-text@1"]["signature"],
            "{path: text} -> [text]"
        );

        let round_tripped: IndexFile =
            serde_json::from_str(&json).expect("must deserialize what we just serialized");
        assert_eq!(round_tripped.version, INDEX_VERSION);
        assert!(round_tripped.entries.contains_key("chunk-text@1"));
    }
}
