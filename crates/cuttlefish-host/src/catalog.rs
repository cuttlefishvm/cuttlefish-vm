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
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current schema version of `index.json`'s own on-disk format — bumped only
/// when the *shape* of the index changes, never tied to this crate's version.
// Only reachable through `read_index`/`with_locked_index`, which are
// themselves only called from tests until Task 9 wires `Catalog::add` in.
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
// Only reachable through `read_index`/`with_locked_index`, which are
// themselves only called from tests until Task 9 wires `Catalog::add` in.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

impl IndexFile {
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
    #[error(
        "no such catalog entry: {name_version}{}",
        format_did_you_mean(did_you_mean)
    )]
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
    let target_name = target_name.split('@').next().unwrap_or(target_name);
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

// Wired into Catalog::add starting in Task 9.
#[allow(dead_code)]
const WASM_MAGIC: &[u8] = b"\0asm";
// Wired into Catalog::add starting in Task 9.
#[allow(dead_code)]
const BUNDLE_MAGIC: &[u8] = b"CFBD";

/// Identify an artifact by its magic bytes, never by file extension — the
/// same "classify by content" rule `handles::classify` already applies to
/// input files. `None` means neither magic matched; the caller turns that
/// into `CatalogError::UnrecognizedArtifact`, naming the path.
// Wired into Catalog::add starting in Task 9.
#[allow(dead_code)]
fn sniff_artifact_kind(bytes: &[u8]) -> Option<ArtifactKind> {
    if bytes.starts_with(WASM_MAGIC) {
        Some(ArtifactKind::Block)
    } else if bytes.starts_with(BUNDLE_MAGIC) {
        Some(ArtifactKind::Bundle)
    } else {
        None
    }
}

// Only called from tests and from `with_locked_index` until Task 9 wires
// `Catalog::add` (and later tasks wire `list`/`show`/`rm`) in.
#[allow(dead_code)]
fn index_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

// Only called from `with_locked_index` until Task 9 wires `Catalog::add` in.
#[allow(dead_code)]
fn lock_path(root: &Path) -> PathBuf {
    root.join("index.json.lock")
}

/// Read `index.json`. A missing file is a brand-new, empty catalog — not an
/// error. A file that exists but fails to parse, or whose `version` this
/// build doesn't understand, is `CatalogError::CorruptIndex` — loud, never
/// silently treated as empty (an empty-looking catalog after real entries
/// were written would make every subsequent `add` "work" while quietly
/// discarding everything that came before it).
// Only called from tests and from `with_locked_index` until Task 9 wires
// `Catalog::add` (and later tasks wire `list`/`show`/`rm`) in.
#[allow(dead_code)]
fn read_index(root: &Path) -> Result<IndexFile, CatalogError> {
    let path = index_path(root);
    if !path.exists() {
        return Ok(IndexFile::empty());
    }

    let bytes = fs::read(&path)?;
    let index: IndexFile =
        serde_json::from_slice(&bytes).map_err(|e| CatalogError::CorruptIndex {
            path: path.clone(),
            reason: e.to_string(),
        })?;

    if index.version != INDEX_VERSION {
        return Err(CatalogError::CorruptIndex {
            path,
            reason: format!(
                "index format version {} is not supported by this build (expected {INDEX_VERSION})",
                index.version
            ),
        });
    }

    Ok(index)
}

/// Acquire the exclusive lock on `index.json.lock`, read-modify-write
/// `index.json` atomically (write to a temp file, `fsync`, then rename), and
/// return. The temp-file-then-rename means a reader racing this write always
/// sees either the fully-old or fully-new file, never a partial one — reads
/// (`list`/`show`) never need to take the lock at all.
///
/// The lock is released by `lock_file` simply going out of scope (an
/// OS-level advisory lock is tied to the open file handle) on every return
/// path, including the early return from `f(&mut index)?` below — there is
/// no separate `unlock()` call to forget on an error path.
// Only called from tests until Task 9 wires `Catalog::add` in.
#[allow(dead_code)]
fn with_locked_index<T>(
    root: &Path,
    f: impl FnOnce(&mut IndexFile) -> Result<T, CatalogError>,
) -> Result<T, CatalogError> {
    fs::create_dir_all(root)?;
    let lock_file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(root))?;
    lock_file.lock()?;

    let mut index = read_index(root)?;
    let result = f(&mut index)?;

    let tmp_path = root.join("index.json.tmp");
    let bytes = serde_json::to_vec_pretty(&index).expect("IndexFile always serializes");
    {
        let mut tmp = File::create(&tmp_path)?;
        tmp.write_all(&bytes)?;
        tmp.sync_all()?;
    }
    fs::rename(&tmp_path, index_path(root))?;

    Ok(result)
}

// Only called from tests until Task 9 wires `Catalog::add` in.
#[allow(dead_code)]
fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

/// Write `bytes` into the content-addressed blob store, deduplicating by
/// hash (two names cataloging identical bytes cost nothing extra), and
/// return the hash as `sha256:<hex>` — self-describing in the index even
/// though the on-disk filename is bare hex (it's already inside a directory
/// named `blobs`; a prefix there would be redundant).
// Only called from tests until Task 9 wires `Catalog::add` in.
#[allow(dead_code)]
fn write_blob(root: &Path, bytes: &[u8]) -> Result<String, CatalogError> {
    use sha2::{Digest, Sha256};

    let hex = format!("{:x}", Sha256::digest(bytes));
    let dir = blobs_dir(root);
    fs::create_dir_all(&dir)?;

    let blob_path = dir.join(&hex);
    if !blob_path.exists() {
        let tmp_path = dir.join(format!("{hex}.tmp"));
        {
            let mut tmp = File::create(&tmp_path)?;
            tmp.write_all(bytes)?;
            tmp.sync_all()?;
        }
        fs::rename(&tmp_path, &blob_path)?;
    }

    Ok(format!("sha256:{hex}"))
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
        entries.insert(
            "summarize@1".to_string(),
            entry_fixture("2026-01-01T00:00:00Z"),
        );
        assert_eq!(
            pick_did_you_mean("summarise", &entries),
            vec!["summarize@1".to_string()]
        );
    }

    #[test]
    fn did_you_mean_is_empty_when_nothing_registered_is_close() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "summarize@1".to_string(),
            entry_fixture("2026-01-01T00:00:00Z"),
        );
        assert!(pick_did_you_mean("completely-unrelated-name", &entries).is_empty());
    }

    #[test]
    fn did_you_mean_is_capped_at_five_closest_ordered_by_distance() {
        let mut entries = BTreeMap::new();
        // All within edit distance 1 of "cat" by construction (each swaps one
        // letter), so the cap — not the distance threshold — is what's under test.
        for (i, name) in ["bat", "cot", "car", "cap", "can", "cad"]
            .iter()
            .enumerate()
        {
            entries.insert(
                format!("{name}@1"),
                entry_fixture(&format!("2026-01-0{}T00:00:00Z", i + 1)),
            );
        }
        let suggestions = pick_did_you_mean("cat", &entries);
        assert_eq!(suggestions.len(), 5, "capped at 5: {suggestions:?}");
    }

    #[test]
    fn did_you_mean_suggests_the_newest_version_when_multiple_versions_of_a_close_name_exist() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "summarize@1".to_string(),
            entry_fixture("2026-01-01T00:00:00Z"),
        );
        entries.insert(
            "summarize@2".to_string(),
            entry_fixture("2026-06-01T00:00:00Z"),
        );
        assert_eq!(
            pick_did_you_mean("summarise", &entries),
            vec!["summarize@2".to_string()],
            "must suggest the newest version of a matching name, not every version"
        );
    }

    #[test]
    fn wasm_magic_bytes_sniff_as_a_block() {
        assert_eq!(
            sniff_artifact_kind(b"\0asm\x01\x00\x00\x00"),
            Some(ArtifactKind::Block)
        );
    }

    #[test]
    fn bundle_magic_bytes_sniff_as_a_bundle() {
        assert_eq!(
            sniff_artifact_kind(b"CFBD\x00\x00\x00\x00\x00\x00\x00\x00"),
            Some(ArtifactKind::Bundle)
        );
    }

    #[test]
    fn unrecognised_bytes_sniff_to_none_not_a_guess() {
        assert_eq!(sniff_artifact_kind(b"whatever-this-is"), None);
    }

    #[test]
    fn writing_then_reading_the_index_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        with_locked_index(dir.path(), |index| {
            index
                .entries
                .insert("a@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
            Ok::<_, CatalogError>(())
        })
        .unwrap();

        let index = read_index(dir.path()).unwrap();
        assert!(index.entries.contains_key("a@1"));
    }

    #[test]
    fn reading_an_index_that_does_not_exist_yet_is_an_empty_catalog_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let index = read_index(dir.path()).expect("no index.json yet is not corruption");
        assert!(index.entries.is_empty());
    }

    #[test]
    fn a_truncated_index_is_a_corrupt_index_error_not_an_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("index.json"), b"{\"version\": 1, \"ent").unwrap();

        let err = read_index(dir.path()).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptIndex { .. }),
            "a truncated index must be a loud CorruptIndex, not treated as empty: {err:?}"
        );
    }

    #[test]
    fn an_unsupported_index_version_is_a_corrupt_index_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            br#"{"version": 999, "entries": {}}"#,
        )
        .unwrap();

        let err = read_index(dir.path()).unwrap_err();
        assert!(matches!(err, CatalogError::CorruptIndex { .. }), "{err:?}");
    }

    #[test]
    fn concurrent_writes_from_two_threads_both_land_and_the_index_stays_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().to_path_buf();
        let root_b = dir.path().to_path_buf();

        let t1 = std::thread::spawn(move || {
            with_locked_index(&root_a, |index| {
                index
                    .entries
                    .insert("a@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
                Ok::<_, CatalogError>(())
            })
            .unwrap();
        });
        let t2 = std::thread::spawn(move || {
            with_locked_index(&root_b, |index| {
                index
                    .entries
                    .insert("b@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
                Ok::<_, CatalogError>(())
            })
            .unwrap();
        });
        t1.join().unwrap();
        t2.join().unwrap();

        let index = read_index(dir.path()).expect("the index must still parse after contention");
        assert!(index.entries.contains_key("a@1"));
        assert!(index.entries.contains_key("b@1"));
    }

    #[test]
    fn identical_bytes_under_two_writes_produce_exactly_one_blob_file() {
        let dir = tempfile::tempdir().unwrap();
        let hash1 = write_blob(dir.path(), b"hello world").unwrap();
        let hash2 = write_blob(dir.path(), b"hello world").unwrap();

        assert_eq!(hash1, hash2);
        assert!(hash1.starts_with("sha256:"));

        let blob_count = std::fs::read_dir(dir.path().join("blobs")).unwrap().count();
        assert_eq!(
            blob_count, 1,
            "identical bytes must dedupe to a single blob file"
        );
    }

    #[test]
    fn the_blob_filename_on_disk_is_bare_hex_no_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let hash = write_blob(dir.path(), b"hello world").unwrap();
        let hex = hash
            .strip_prefix("sha256:")
            .expect("index field is prefixed");

        assert!(dir.path().join("blobs").join(hex).exists());
    }
}
