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
// Used starting in Task 6 (index read/write); unused until then.
#[allow(dead_code)]
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
// Used starting in Task 6 (index read/write); unused until then.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

impl IndexFile {
    // Used starting in Task 6 (index read/write); unused until then.
    #[allow(dead_code)]
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// Something went wrong reading, writing, or resolving through the catalog.
#[derive(Debug, thiserror::Error)]
// Constructed for real starting in Task 6 onward (index read/write, resolution).
#[allow(dead_code)]
pub enum CatalogError {
    /// `name@version` was already catalogued; versions are immutable once published.
    #[error("{name_version} is already catalogued; versions are immutable once published")]
    AlreadyExists {
        /// The name@version that was already present.
        name_version: String,
    },
    /// No entry matches the requested `name@version`.
    #[error("no such catalog entry: {name_version}{}", format_did_you_mean(did_you_mean))]
    NotFound {
        /// The name@version that was requested.
        name_version: String,
        /// Names within edit distance 2 of the requested one, closest first,
        /// capped at 5. Empty when nothing is close.
        did_you_mean: Vec<String>,
    },
    /// An unqualified name was used somewhere that requires an exact version
    /// (resolving a node reference already recorded inside a bundle's
    /// manifest — see `ResolutionContext::Durable`).
    #[error("{name} has no version — an exact name@version is required here")]
    UnqualifiedName {
        /// The unqualified name that was rejected.
        name: String,
    },
    /// The catalog's own `index.json` failed to parse.
    #[error("catalog index at {path} is corrupt: {reason}")]
    CorruptIndex {
        /// Path to the unreadable index file.
        path: PathBuf,
        /// What went wrong parsing it.
        reason: String,
    },
    /// The artifact's magic bytes match neither a wasm module nor a bundle.
    #[error("{path}: not a recognised artifact (header: {header:02x?})")]
    UnrecognizedArtifact {
        /// The path that was handed to `add`.
        path: PathBuf,
        /// The first bytes actually seen.
        header: Vec<u8>,
    },
    /// The artifact's magic bytes were recognised, but its contents could not
    /// be read: a wasm-magic file whose module body is truncated or
    /// otherwise invalid, or a bundle-magic file whose manifest JSON fails to
    /// parse. Distinct from `CorruptIndex` (that's the catalog's own
    /// bookkeeping file, not an input artifact) and from
    /// `UnrecognizedArtifact` (that's the magic byte itself not matching
    /// anything).
    #[error("{path}: {reason}")]
    UninspectableArtifact {
        /// The path that was handed to `add`.
        path: PathBuf,
        /// What went wrong reading past the magic bytes.
        reason: String,
    },
    /// Underlying I/O failure — a plain failure to open/read/write a path,
    /// before any catalog-specific logic ever inspects the bytes.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Render `did_you_mean` as a message suffix, or nothing if it's empty —
/// never a dangling "(did you mean: ?)" for a genuinely unmatched name.
// Used by CatalogError::NotFound; constructed for real starting in Task 6 onward.
#[allow(dead_code)]
fn format_did_you_mean(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(" (did you mean: {}?)", names.join(", "))
    }
}

/// Levenshtein edit distance between two strings, by character.
// Wired into Catalog::show/resolve starting in Task 10; used in tests until then.
#[allow(dead_code)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];

    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Pick up to 5 catalogued names within edit distance 2 of `target_name`
/// (compared as bare names, with any `@version` stripped from both sides),
/// closest first, ties broken by `created_at` ascending (oldest first). A
/// prefix match misses common real typos (`summarise`/`summarize` share no
/// prefix relationship); edit distance catches them.
// Wired into Catalog::show/rm/resolve starting in Task 10; used in tests until then.
#[allow(dead_code)]
fn pick_did_you_mean(target_name: &str, entries: &BTreeMap<String, Entry>) -> Vec<String> {
    const MAX_DISTANCE: usize = 2;
    const LIMIT: usize = 5;

    // Suggest the newest version of each close name, not every version of it.
    let mut by_name: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for (name_version, entry) in entries {
        let name = name_version.split('@').next().unwrap_or(name_version);
        if levenshtein(target_name, name) > MAX_DISTANCE {
            continue;
        }
        by_name
            .entry(name)
            .and_modify(|(nv, created)| {
                if entry.created_at.as_str() > *created {
                    *nv = name_version;
                    *created = entry.created_at.as_str();
                }
            })
            .or_insert((name_version, entry.created_at.as_str()));
    }

    let mut candidates: Vec<(usize, &str, &str)> = by_name
        .into_iter()
        .map(|(name, (nv, created))| (levenshtein(target_name, name), nv, created))
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(b.2)));

    candidates
        .into_iter()
        .take(LIMIT)
        .map(|(_, nv, _)| nv.to_string())
        .collect()
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

    #[test]
    fn not_found_with_suggestions_reads_as_one_sentence() {
        let err = CatalogError::NotFound {
            name_version: "summarise@1".to_string(),
            did_you_mean: vec!["summarize@1".to_string()],
        };
        assert_eq!(
            err.to_string(),
            "no such catalog entry: summarise@1 (did you mean: summarize@1?)"
        );
    }

    #[test]
    fn not_found_with_no_suggestions_has_no_dangling_parenthetical() {
        let err = CatalogError::NotFound {
            name_version: "xyz@1".to_string(),
            did_you_mean: vec![],
        };
        assert_eq!(err.to_string(), "no such catalog entry: xyz@1");
    }

    fn entry_fixture(created_at: &str) -> Entry {
        Entry {
            hash: "sha256:deadbeef".to_string(),
            kind: ArtifactKind::Block,
            signature: "json -> json".to_string(),
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("summarize", "summarise"), 1);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn did_you_mean_catches_a_one_character_typo_a_prefix_match_would_miss() {
        // "summarise" and "summarize" share no prefix relationship (they diverge
        // at the 8th character) — a starts-with prefix match would silently
        // produce zero suggestions on exactly this typo.
        let mut entries = BTreeMap::new();
        entries.insert("summarize@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
        assert_eq!(
            pick_did_you_mean("summarise", &entries),
            vec!["summarize@1".to_string()]
        );
    }

    #[test]
    fn did_you_mean_is_empty_when_nothing_registered_is_close() {
        let mut entries = BTreeMap::new();
        entries.insert("summarize@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
        assert!(pick_did_you_mean("completely-unrelated-name", &entries).is_empty());
    }

    #[test]
    fn did_you_mean_is_capped_at_five_closest_ordered_by_distance() {
        let mut entries = BTreeMap::new();
        // All within edit distance 1 of "cat" by construction (each swaps one
        // letter), so the cap — not the distance threshold — is what's under test.
        for (i, name) in ["bat", "cot", "car", "cap", "can", "cad"].iter().enumerate() {
            entries.insert(
                format!("{name}@1"),
                entry_fixture(&format!("2026-01-0{}T00:00:00Z", i + 1)),
            );
        }
        let suggestions = pick_did_you_mean("cat", &entries);
        assert_eq!(suggestions.len(), 5, "capped at 5: {suggestions:?}");
    }
}
