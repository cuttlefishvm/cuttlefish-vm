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
const INDEX_VERSION: u32 = 1;

/// Whether a cataloged artifact is a single wasm block or a multi-node bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A single compiled wasm module.
    Block,
    /// A `.cfbundle` container produced by `cuttlefish build`.
    Bundle,
    /// A Rhai script, run by the shared interpreter — see
    /// `pipeline::resolve_and_load`'s `Script` handling for how this
    /// becomes runnable.
    Script,
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
    /// `name@version` -> the hash it was published with, for versions that
    /// have since been removed. Keeps `rm` from being a way to launder a
    /// republish: the identity stays claimed by its original content even
    /// after the entry is gone.
    ///
    /// `default` rather than an `INDEX_VERSION` bump on purpose — an index
    /// written before this field existed is a perfectly good version-1 index
    /// with nothing retired, and bumping the version would make every
    /// already-written catalog report itself as corrupt.
    #[serde(default)]
    retired: BTreeMap<String, String>,
}

impl IndexFile {
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            entries: BTreeMap::new(),
            retired: BTreeMap::new(),
        }
    }
}

/// Characters legal in either half of a `name@version`. Deliberately narrow:
/// a catalog identifier is a key people type and scripts interpolate, so
/// whitespace and path separators earn nothing and invite confusion between
/// an identifier and a filesystem path.
fn is_legal_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// Check that `s` is a well-formed `name@version` before it can be written
/// into the index.
///
/// This guards the write path only. `show`/`rm`/`resolve` stay permissive so
/// that a key already in an index — hand-edited, or written before this check
/// existed — remains inspectable and removable rather than stranded.
fn validate_name_version(s: &str) -> Result<(), CatalogError> {
    let invalid = |reason: String| CatalogError::InvalidNameVersion {
        name_version: s.to_string(),
        reason,
    };

    let separators = s.matches('@').count();
    if separators != 1 {
        return Err(invalid(match separators {
            0 => "expected <name>@<version>, e.g. echo-summarize@1".to_string(),
            n => format!("found {n} '@' separators"),
        }));
    }

    let (name, version) = s.split_once('@').expect("exactly one '@' is present");
    if name.is_empty() {
        return Err(invalid("the name is empty".to_string()));
    }
    if version.is_empty() {
        return Err(invalid("the version is empty".to_string()));
    }

    for (half, label) in [(name, "name"), (version, "version")] {
        if let Some(bad) = half.chars().find(|c| !is_legal_identifier_char(*c)) {
            return Err(invalid(format!(
                "the {label} contains {bad:?}; only letters, digits, '.', '-' and '_' are allowed"
            )));
        }
    }

    Ok(())
}

/// Reserved Windows device basenames — case-insensitive, and reserved
/// regardless of any extension (`con.txt` is just as invalid as `con`).
/// None of these can be created as a directory or file name on Windows.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate a `cuttlefish block new` name: must be legal as a Cargo
/// crate-name component (so `cf-block-<name>` is always a valid package
/// name for the Rust authoring path, even when a script is what actually
/// gets scaffolded) and safe as a directory basename on every platform this
/// project supports.
///
/// Deliberately **not** the same validator as [`validate_name_version`]'s
/// name half: that one allows `.`, which Cargo forbids outright in a crate
/// name — confirmed empirically (`cargo init --name "cf-block-my.block"`
/// fails) during this feature's design. A block name must be valid under
/// *both* authoring paths, not just whichever one happens to be requested,
/// so a name is never valid under `--lang rhai` and invalid under
/// `--lang rust`.
pub fn validate_block_name(name: &str) -> Result<(), CatalogError> {
    let invalid = |reason: String| CatalogError::InvalidBlockName {
        name: name.to_string(),
        reason,
    };

    if name.is_empty() {
        return Err(invalid("the name is empty".to_string()));
    }
    if !name.chars().next().unwrap().is_ascii_alphabetic() {
        return Err(invalid("must start with a letter".to_string()));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    {
        return Err(invalid(format!(
            "contains {bad:?}; only letters, digits, '-' and '_' are allowed"
        )));
    }
    if WINDOWS_RESERVED_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(invalid(format!(
            "\"{name}\" is a reserved name on Windows and can't be used as a directory name there"
        )));
    }

    Ok(())
}

/// Something went wrong reading, writing, or resolving through the catalog.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// `name@version` was already catalogued; versions are immutable once published.
    #[error("{name_version} is already catalogued; versions are immutable once published")]
    AlreadyExists {
        /// The name@version that was already present.
        name_version: String,
    },
    /// The identifier handed to `add` is not a well-formed `name@version`.
    #[error("{name_version:?} is not a name@version ({reason})")]
    InvalidNameVersion {
        /// The identifier that was rejected.
        name_version: String,
        /// Why it was rejected, as a sentence fragment.
        reason: String,
    },
    /// The name handed to `cuttlefish block new` isn't legal as a directory
    /// basename on every platform this project supports, or wouldn't survive
    /// as the crate-name component of `cf-block-<name>` under the Rust
    /// authoring path. Distinct from `InvalidNameVersion`: a block name never
    /// has an `@version` half, so reusing that variant's "is not a
    /// name@version" wording here would misdescribe the failure.
    #[error("{name:?} is not a valid block name ({reason})")]
    InvalidBlockName {
        /// The name that was rejected.
        name: String,
        /// Why it was rejected, as a sentence fragment.
        reason: String,
    },
    /// `name@version` was published, then removed, and is now being re-added
    /// with different content. Removing an entry drops it from the index but
    /// does not un-publish the identity, so this is still the immutability
    /// violation `AlreadyExists` guards against — just spread over two steps.
    #[error(
        "{name_version} was previously catalogued with different content; versions are \
         immutable once published ({previous_hash} -> {new_hash})"
    )]
    RetiredWithDifferentContent {
        /// The name@version being re-added.
        name_version: String,
        /// The hash it was published with originally.
        previous_hash: String,
        /// The hash of the artifact now being offered.
        new_hash: String,
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
        /// The path that was handed to `add`, or a synthetic label standing
        /// in for one — `read_bundle_signature` takes a `label: &str` that
        /// need not be a real filesystem path. `add` always passes a real
        /// on-disk path, but `pipeline::check` calls it with a stage's
        /// display name, which for a `Cataloged` stage (built by
        /// `pipeline::resolve_and_load` from a bare catalog name, not a
        /// filesystem path) is just that bare name.
        path: PathBuf,
        /// What went wrong reading past the magic bytes.
        reason: String,
    },
    /// An entry's `hash` came back from `index.json` in a shape that isn't a
    /// well-formed sha256 digest (64 lowercase hex digits, optionally
    /// prefixed with `sha256:`) — the exact shape `write_blob` always
    /// produces. `index.json` is never format-validated on read, so a
    /// hand-edited or maliciously crafted index could otherwise smuggle
    /// `../` traversal or an absolute path into a filesystem join; this is
    /// the guard that rejects it before that ever happens. Distinct from
    /// `CorruptIndex` (that's `index.json` failing to *parse* as JSON at
    /// all; this is JSON that parses fine but whose `hash` field is
    /// nonsense).
    #[error(
        "catalog entry has a malformed hash {hash:?}: expected sha256:<64 lowercase hex digits>"
    )]
    MalformedHash {
        /// The invalid hash value, exactly as found in the entry.
        hash: String,
    },
    /// Underlying I/O failure — a plain failure to open/read/write a path,
    /// before any catalog-specific logic ever inspects the bytes.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Render `did_you_mean` as a message suffix, or nothing if it's empty —
/// never a dangling "(did you mean: ?)" for a genuinely unmatched name.
fn format_did_you_mean(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(" (did you mean: {}?)", names.join(", "))
    }
}

/// Levenshtein edit distance between two strings, by character.
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

const WASM_MAGIC: &[u8] = b"\0asm";
/// The one source of truth for the `.cfbundle` container's magic bytes —
/// shared with `bundle::build` (the writer) so the two can never drift
/// apart the way `manifest_len`'s endianness once had to be pinned down
/// after the fact.
pub(crate) const BUNDLE_MAGIC: &[u8; 4] = b"CFBD";
/// `.cfbundle`'s fixed header size: `BUNDLE_MAGIC` (4 bytes) + `manifest_len`
/// as a little-endian `u64` (8 bytes). Shared with `bundle::build`, which
/// writes exactly this many bytes before the manifest.
pub(crate) const BUNDLE_HEADER_LEN: usize = BUNDLE_MAGIC.len() + 8;

/// Identify an artifact by its magic bytes, never by file extension — the
/// same "classify by content" rule `handles::classify` already applies to
/// input files. `None` means neither magic matched; the caller turns that
/// into `CatalogError::UnrecognizedArtifact`, naming the path.
pub(crate) fn sniff_artifact_kind(bytes: &[u8]) -> Option<ArtifactKind> {
    if bytes.starts_with(WASM_MAGIC) {
        Some(ArtifactKind::Block)
    } else if bytes.starts_with(BUNDLE_MAGIC) {
        Some(ArtifactKind::Bundle)
    } else {
        None
    }
}

fn index_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

fn lock_path(root: &Path) -> PathBuf {
    root.join("index.json.lock")
}

/// Read `index.json`. A missing file is a brand-new, empty catalog — not an
/// error. A file that exists but fails to parse, or whose `version` this
/// build doesn't understand, is `CatalogError::CorruptIndex` — loud, never
/// silently treated as empty (an empty-looking catalog after real entries
/// were written would make every subsequent `add` "work" while quietly
/// discarding everything that came before it).
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

fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

/// Whether `hex` is exactly 64 lowercase hex digits — the exact shape
/// `write_blob` always produces via `format!("{:x}", Sha256::digest(bytes))`.
/// This is the sole guard between an `Entry`'s `hash` field (read straight
/// out of `index.json`, never format-validated elsewhere) and a filesystem
/// path: without it, a hash like `../../../etc/passwd` or an absolute path
/// would `Path::join` straight through to arbitrary files outside `blobs/`.
fn is_well_formed_sha256_hex(hex: &str) -> bool {
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Read just enough of a `.cfbundle` container (magic already confirmed by
/// the caller) to recover its cached `signature` field — never touching the
/// stage blobs that follow the manifest, since the catalog only ever needs
/// the signature.
///
/// `manifest_len` is **little-endian** — the build spike documents it only
/// as `u64`, with no endianness stated, so this is the first concrete
/// commitment to a byte order for the container format. Pinned here rather
/// than left ambiguous a second time: whoever implements `cuttlefish build`
/// (which doesn't exist as code yet) must write `manifest_len` little-endian
/// to match what this reader expects, not native-endian.
pub(crate) fn read_bundle_signature(bytes: &[u8], label: &str) -> Result<String, CatalogError> {
    if bytes.len() < BUNDLE_HEADER_LEN {
        return Err(CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: "shorter than the bundle header".to_string(),
        });
    }

    let manifest_len =
        u64::from_le_bytes(bytes[4..BUNDLE_HEADER_LEN].try_into().expect("8 bytes")) as usize;
    let manifest_bytes = BUNDLE_HEADER_LEN
        .checked_add(manifest_len)
        .and_then(|end| bytes.get(BUNDLE_HEADER_LEN..end))
        .ok_or_else(|| CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: format!("manifest_len {manifest_len} exceeds the file's actual length"),
        })?;

    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes).map_err(|e| {
        CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: format!("manifest is not valid JSON: {e}"),
        }
    })?;

    // A node whose offset+len falls outside the stage-bytes region that
    // actually follows the manifest is structurally impossible — it can
    // only come from a hand-crafted or corrupt bundle, since `bundle::build`
    // always writes offsets that fit. Reject it now, at the one point every
    // bundle (this crate's own output or someone else's) passes through
    // before being embedded in something else or cataloged, rather than
    // leaving it to be discovered later as an out-of-bounds read once
    // nested-subjobs actually executes a node.
    if let Some(nodes) = manifest.get("nodes").and_then(|v| v.as_array()) {
        let body_len = (bytes.len() - BUNDLE_HEADER_LEN - manifest_len) as u64;
        for node in nodes {
            let bounds = node
                .get("offset")
                .and_then(|v| v.as_u64())
                .zip(node.get("len").and_then(|v| v.as_u64()));
            let in_bounds = match bounds {
                Some((offset, len)) => offset.checked_add(len).is_some_and(|end| end <= body_len),
                None => false,
            };
            if !in_bounds {
                return Err(CatalogError::UninspectableArtifact {
                    path: PathBuf::from(label),
                    reason: format!(
                        "a node's offset/len ({:?}) doesn't fit within the bundle's \
                         {body_len}-byte stage-bytes region",
                        (
                            node.get("offset").and_then(|v| v.as_u64()),
                            node.get("len").and_then(|v| v.as_u64())
                        )
                    ),
                });
            }
        }
    }

    manifest
        .get("signature")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: "manifest has no string field \"signature\"".to_string(),
        })
}

/// Recover a `.rhai` script's declared signature from its `//! signature:
/// ...` header comment — the same role `read_bundle_signature` plays for a
/// `.cfbundle`'s manifest, and re-derived every time it's needed rather
/// than cached, for the same reason: the artifact itself is the source of
/// truth, never a copy that could drift from it.
///
/// `block new`'s Rhai template writes this header at scaffold time (a later
/// task); this function is what reads it back, both here at `add`-time (to
/// populate `Entry.signature` for `list`/`show`) and later, identically, in
/// `pipeline::read_stage_signature`'s `Script` arm (to typecheck the node
/// at spec-load time).
pub(crate) fn read_script_signature(bytes: &[u8], label: &str) -> Result<String, CatalogError> {
    let text = std::str::from_utf8(bytes).map_err(|_| CatalogError::UninspectableArtifact {
        path: PathBuf::from(label),
        reason: "not valid UTF-8 text".to_string(),
    })?;
    let header = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("//! signature:"))
        .ok_or_else(|| CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: "no `//! signature: <input> -> <output>` header comment found".to_string(),
        })?
        .trim();

    // Round-trip through the real parser now, not just at check-time, so a
    // malformed header is caught at `catalog add` — the earliest point a
    // human or agent can act on it — rather than surfacing later as a
    // confusing failure when a spec referencing this entry is checked.
    header.parse::<cuttlefish_abi::Signature>().map_err(|e| {
        CatalogError::UninspectableArtifact {
            path: PathBuf::from(label),
            reason: format!("signature header `{header}` does not parse: {e}"),
        }
    })?;

    Ok(header.to_string())
}

/// Write `bytes` into the content-addressed blob store, deduplicating by
/// hash (two names cataloging identical bytes cost nothing extra), and
/// return the hash as `sha256:<hex>` — self-describing in the index even
/// though the on-disk filename is bare hex (it's already inside a directory
/// named `blobs`; a prefix there would be redundant).
fn write_blob(root: &Path, bytes: &[u8]) -> Result<String, CatalogError> {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    let hex = format!("{:x}", Sha256::digest(bytes));
    let dir = blobs_dir(root);
    fs::create_dir_all(&dir)?;

    let blob_path = dir.join(&hex);
    if !blob_path.exists() {
        // Unique per call (process id + a process-lifetime counter), not
        // derived from the hash — two concurrent writers of identical bytes
        // must never share a temp path. A deterministic {hex}.tmp name let
        // one writer's O_TRUNC-on-open truncate the other's in-flight or
        // already-written-but-not-yet-renamed file, risking a truncated
        // file landing at blob_path under a hash it doesn't actually match.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dir.join(format!("{hex}.tmp.{}.{unique}", std::process::id()));
        {
            let mut tmp = File::create(&tmp_path)?;
            tmp.write_all(bytes)?;
            tmp.sync_all()?;
        }
        fs::rename(&tmp_path, &blob_path)?;
    }

    Ok(format!("sha256:{hex}"))
}

/// A local block catalog rooted at a directory. This type has no opinion
/// about environment variables or home directories — the caller (the CLI)
/// decides where `root` points.
pub struct Catalog {
    root: PathBuf,
}

/// What `add` actually did, for a caller to report.
#[derive(Debug, Clone)]
pub struct AddOutcome {
    /// The name@version now cataloged.
    pub name_version: String,
    /// Block or bundle.
    pub kind: ArtifactKind,
    /// The cached signature string.
    pub signature: String,
    /// True when `signature` is the permissive `json -> json` default — the
    /// caller should print a warning, not fail; the permissive fallback
    /// itself is existing, intentional behavior, unchanged by the catalog.
    pub is_permissive_default: bool,
}

/// Where a pipeline entry string is being resolved from — determines whether
/// an unqualified name (no `@version`) is legal. The dividing line is
/// source-spec-text vs. compiled-artifact-reference, not "which command
/// invoked it": `cuttlefish build` resolves an unqualified name from the
/// spec it was given exactly once, at build time, and records the exact
/// resolution in the manifest it emits — the unqualified form itself never
/// survives into that manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionContext {
    /// Resolving a reference found directly in a source `.cuttlefish` spec,
    /// on behalf of a top-level interactive command (`cuttlefish run`, or
    /// `cuttlefish build` pointed at that spec file). Unqualified names are
    /// legal here.
    Interactive,
    /// Resolving a node reference already recorded inside a bundle's
    /// manifest. Unqualified names are illegal here.
    Durable,
}

/// The result of resolving one pipeline entry string.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// `s` was a direct filesystem path or ended in `.wasm` — used as-is, no
    /// catalog lookup at all.
    Direct(PathBuf),
    /// `s` resolved through the catalog to this entry.
    Cataloged {
        /// The exact name@version resolved to, even if `s` itself was
        /// unqualified.
        name_version: String,
        /// The resolved entry.
        entry: Entry,
    },
}

impl Catalog {
    /// Open (without yet creating on disk) a catalog rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Catalog the artifact at `artifact_path` under `name_version`.
    ///
    /// `engine` is only used if the artifact turns out to be a wasm block —
    /// a bundle's signature is read straight from its own manifest, never
    /// wasm-instantiated (bundle bytes are a custom container, not a wasm
    /// module; instantiating them would simply fail to parse).
    pub fn add(
        &self,
        name_version: &str,
        artifact_path: &Path,
        engine: &wasmtime::Engine,
    ) -> Result<AddOutcome, CatalogError> {
        validate_name_version(name_version)?;

        let bytes = fs::read(artifact_path)?;
        let kind = match sniff_artifact_kind(&bytes) {
            Some(k) => k,
            None if artifact_path.extension().is_some_and(|e| e == "rhai") => ArtifactKind::Script,
            None => {
                return Err(CatalogError::UnrecognizedArtifact {
                    path: artifact_path.to_path_buf(),
                    header: bytes.iter().take(8).copied().collect(),
                })
            }
        };

        let (signature, is_permissive_default) = match kind {
            ArtifactKind::Block => {
                let sig = crate::runner::read_signature(engine, &bytes).map_err(|e| {
                    CatalogError::UninspectableArtifact {
                        path: artifact_path.to_path_buf(),
                        reason: format!("{e:#}"),
                    }
                })?;
                let permissive = cuttlefish_abi::Signature {
                    input: cuttlefish_abi::Ty::Json,
                    output: cuttlefish_abi::Ty::Json,
                };
                let is_permissive = sig == permissive;
                (sig.to_string(), is_permissive)
            }
            ArtifactKind::Bundle => {
                let sig = read_bundle_signature(&bytes, &artifact_path.to_string_lossy())?;
                (sig, false)
            }
            ArtifactKind::Script => {
                let sig = read_script_signature(&bytes, &artifact_path.to_string_lossy())?;
                (sig, false)
            }
        };

        let hash = write_blob(&self.root, &bytes)?;
        let created_at = now_rfc3339();
        let name_version = name_version.to_string();

        with_locked_index(&self.root, |index| {
            if index.entries.contains_key(&name_version) {
                return Err(CatalogError::AlreadyExists {
                    name_version: name_version.clone(),
                });
            }
            // A removed version keeps its claim on the identity. Re-adding the
            // exact bytes it was published with is an undo of the `rm`;
            // re-adding anything else is a republish, which is the thing
            // immutability exists to forbid.
            if let Some(previous_hash) = index.retired.get(&name_version) {
                if previous_hash != &hash {
                    return Err(CatalogError::RetiredWithDifferentContent {
                        name_version: name_version.clone(),
                        previous_hash: previous_hash.clone(),
                        new_hash: hash.clone(),
                    });
                }
                index.retired.remove(&name_version);
            }
            index.entries.insert(
                name_version.clone(),
                Entry {
                    hash,
                    kind,
                    signature: signature.clone(),
                    created_at,
                },
            );
            Ok(())
        })?;

        Ok(AddOutcome {
            name_version,
            kind,
            signature,
            is_permissive_default,
        })
    }

    /// List every cataloged entry, in deterministic (sorted-by-name@version)
    /// order.
    pub fn list(&self) -> Result<Vec<(String, Entry)>, CatalogError> {
        let index = read_index(&self.root)?;
        Ok(index.entries.into_iter().collect())
    }

    /// Look up one entry's cached `Entry` by exact, case-sensitive
    /// `name@version` — a catalog name is an opaque string, like a version is;
    /// no case-folding, no normalization.
    pub fn show(&self, name_version: &str) -> Result<Entry, CatalogError> {
        let index = read_index(&self.root)?;
        index.entries.get(name_version).cloned().ok_or_else(|| {
            let name = name_version.split('@').next().unwrap_or(name_version);
            CatalogError::NotFound {
                name_version: name_version.to_string(),
                did_you_mean: pick_did_you_mean(name, &index.entries),
            }
        })
    }

    /// Read an entry's raw bytes back out of the blob store. The catalog's
    /// only way to get from an `Entry` to actual artifact bytes — `blobs/`
    /// stays an implementation detail, same as `add()` already hides
    /// `write_blob`.
    pub fn read_blob(&self, entry: &Entry) -> Result<Vec<u8>, CatalogError> {
        let hex = entry.hash.strip_prefix("sha256:").unwrap_or(&entry.hash);
        if !is_well_formed_sha256_hex(hex) {
            return Err(CatalogError::MalformedHash {
                hash: entry.hash.clone(),
            });
        }
        Ok(fs::read(blobs_dir(&self.root).join(hex))?)
    }

    /// Remove a `name@version` from the index. The blob it pointed at is left on
    /// disk — no garbage collection in v1 (an orphaned blob is wasted space, not
    /// a correctness problem; see the design doc).
    pub fn rm(&self, name_version: &str) -> Result<(), CatalogError> {
        with_locked_index(&self.root, |index| {
            if let Some(entry) = index.entries.remove(name_version) {
                // Record what this identity was published as, so a later
                // `add` can tell an undo from a republish.
                index
                    .retired
                    .insert(name_version.to_string(), entry.hash.clone());
                Ok(())
            } else {
                let name = name_version.split('@').next().unwrap_or(name_version);
                Err(CatalogError::NotFound {
                    name_version: name_version.to_string(),
                    did_you_mean: pick_did_you_mean(name, &index.entries),
                })
            }
        })
    }

    /// Resolve one pipeline entry string per the catalog spec's three-step
    /// algorithm: direct path/`.wasm`/`.cfbundle` first, then an exact
    /// catalog lookup if `@version` is present, then latest-by-`created_at`
    /// if it's not and `context` allows an unqualified name.
    ///
    /// A compiled-artifact suffix (`.wasm` or `.cfbundle`) is always treated
    /// as Direct, even when nothing actually exists at that path — a
    /// genuinely missing artifact should fail with a clear "no such file",
    /// not be silently reinterpreted as a catalog name that happens to
    /// contain a `.` in it. Both suffixes get identical treatment here:
    /// `pipeline::resolve_and_load`'s own decision to prefer a joined path
    /// for one of these suffixes (even before checking existence) is only
    /// correct if this function honors the same suffixes the same way.
    pub fn resolve(&self, s: &str, context: ResolutionContext) -> Result<Resolved, CatalogError> {
        if s.ends_with(".wasm") || s.ends_with(".cfbundle") || Path::new(s).exists() {
            return Ok(Resolved::Direct(PathBuf::from(s)));
        }

        let index = read_index(&self.root)?;

        if let Some((name, version)) = s.rsplit_once('@') {
            let name_version = format!("{name}@{version}");
            let entry = index.entries.get(&name_version).cloned().ok_or_else(|| {
                CatalogError::NotFound {
                    name_version: name_version.clone(),
                    did_you_mean: pick_did_you_mean(name, &index.entries),
                }
            })?;
            return Ok(Resolved::Cataloged {
                name_version,
                entry,
            });
        }

        if context == ResolutionContext::Durable {
            return Err(CatalogError::UnqualifiedName {
                name: s.to_string(),
            });
        }

        let mut versions: Vec<(&String, &Entry)> = index
            .entries
            .iter()
            .filter(|(nv, _)| nv.rsplit_once('@').map(|(n, _)| n) == Some(s))
            .collect();
        versions.sort_by(|a, b| a.1.created_at.cmp(&b.1.created_at));

        let (name_version, entry) =
            versions
                .last()
                .copied()
                .ok_or_else(|| CatalogError::NotFound {
                    name_version: s.to_string(),
                    did_you_mean: pick_did_you_mean(s, &index.entries),
                })?;

        Ok(Resolved::Cataloged {
            name_version: name_version.clone(),
            entry: entry.clone(),
        })
    }
}

/// The bare `$CUTTLEFISH_HOME`/`~/.cuttlefish` root the catalog and most of
/// this crate's other on-disk layout is rooted under (the jobs directory is
/// the exception: it honors `$CUTTLEFISH_JOBS_HOME` first, see
/// `ledger::jobs_root`). `None` when neither `$CUTTLEFISH_HOME` nor a
/// resolvable home directory is available — this library never exits the
/// process on a caller's behalf, so reporting that is the caller's job (both
/// `cuttlefish` and `cuttlefishd` already have their own error-reporting
/// convention).
pub(crate) fn cuttlefish_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CUTTLEFISH_HOME") {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir().map(|home| home.join(".cuttlefish"))
}

/// Where the catalog lives when the caller doesn't say otherwise:
/// `$CUTTLEFISH_HOME/catalog` if set, else `~/.cuttlefish/catalog`. `None`
/// when neither is available — see `cuttlefish_home` (private).
pub fn default_root() -> Option<PathBuf> {
    cuttlefish_home().map(|h| h.join("catalog"))
}

/// The current UTC time, truncated to whole seconds and formatted as RFC
/// 3339 (`2026-08-02T18:03:00Z`). Truncating avoids variable-width fractional
/// seconds, so `created_at` strings sort correctly with plain string
/// comparison (used for "give me the latest" and did-you-mean tie-breaking)
/// without ever needing to be parsed back.
pub(crate) fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("0 is always a valid nanosecond value");
    now.format(&time::format_description::well_known::Rfc3339)
        .expect("Rfc3339 formatting cannot fail for a valid OffsetDateTime")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    /// Enough threads to genuinely contend for the index lock on any machine
    /// this runs on, without making the test slow.
    const WRITERS: usize = 16;

    #[test]
    fn default_root_honors_cuttlefish_home() {
        // No other test in this process mutates CUTTLEFISH_HOME, and
        // catalog.rs's own tests never read it — set/remove is confined to
        // this one test. (This toolchain's std::env::set_var/remove_var are
        // safe fns, not unsafe — the crate forbids unsafe_code entirely, so
        // an unsafe wrapper isn't an option regardless.)
        std::env::set_var("CUTTLEFISH_HOME", "/tmp/cf-test-home");
        let root = default_root();
        std::env::remove_var("CUTTLEFISH_HOME");
        assert_eq!(root, Some(PathBuf::from("/tmp/cf-test-home/catalog")));
    }

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
            retired: BTreeMap::new(),
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

    fn seed(root: &Path, name_version: &str, created_at: &str) {
        with_locked_index(root, |index| {
            index
                .entries
                .insert(name_version.to_string(), entry_fixture(created_at));
            Ok::<_, CatalogError>(())
        })
        .unwrap();
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

    /// `add`'s duplicate check runs *inside* the locked section, so a pack of
    /// writers racing for one key must resolve to exactly one winner — the
    /// "versions are immutable once published" promise only holds under
    /// contention if the check-then-insert is genuinely atomic. Every thread
    /// is held at a barrier so they collide on the lock rather than politely
    /// serializing.
    #[test]
    fn racing_inserts_of_the_same_key_leave_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(WRITERS));

        let handles: Vec<_> = (0..WRITERS)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    with_locked_index(&root, |index| {
                        if index.entries.contains_key("race@1") {
                            return Err(CatalogError::AlreadyExists {
                                name_version: "race@1".to_string(),
                            });
                        }
                        index
                            .entries
                            .insert("race@1".to_string(), entry_fixture("2026-01-01T00:00:00Z"));
                        Ok(())
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            winners, 1,
            "exactly one racing writer may claim a key; got {winners}"
        );
        assert!(
            results
                .iter()
                .all(|r| r.is_ok() || matches!(r, Err(CatalogError::AlreadyExists { .. }))),
            "every loser must lose with AlreadyExists, not an io or corruption error: {results:?}"
        );

        let index = read_index(&root).expect("the index must still parse after contention");
        assert_eq!(index.entries.len(), 1);
    }

    /// The two-thread test above proves the lock exists; this proves it holds
    /// up under real contention. Without a barrier, two threads usually finish
    /// one after another and never touch the lock at the same time, so a
    /// read-modify-write that lost updates could still pass.
    #[test]
    fn many_racing_writers_of_distinct_keys_all_land_with_no_lost_updates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(WRITERS));

        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    with_locked_index(&root, |index| {
                        index.entries.insert(
                            format!("writer-{w}@1"),
                            entry_fixture("2026-01-01T00:00:00Z"),
                        );
                        Ok::<_, CatalogError>(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let index = read_index(&root).expect("the index must still parse after contention");
        assert_eq!(
            index.entries.len(),
            WRITERS,
            "every writer's entry must survive; a lost update means the \
             read-modify-write escaped the lock: {:?}",
            index.entries.keys().collect::<Vec<_>>()
        );
    }

    /// `with_locked_index` documents that readers need no lock because a
    /// writer only ever publishes via rename, so a reader sees the wholly-old
    /// or wholly-new file and never a half-written one. Nothing asserted it:
    /// this hammers lock-free readers against writers and fails if any read
    /// ever comes back corrupt.
    #[test]
    fn lock_free_readers_never_observe_a_partial_index_while_writers_hammer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Seed first so index.json exists before any reader starts — a
        // missing index is legitimately empty, which would mask a torn read.
        seed(&root, "seed@1", "2026-01-01T00:00:00Z");

        let stop = Arc::new(AtomicBool::new(false));

        let writers: Vec<_> = (0..4)
            .map(|w| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..60 {
                        with_locked_index(&root, |index| {
                            index.entries.insert(
                                format!("w{w}-{i}@1"),
                                entry_fixture("2026-01-01T00:00:00Z"),
                            );
                            Ok::<_, CatalogError>(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut reads = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let index = read_index(&root)
                            .expect("a lock-free reader must never see a partial or corrupt index");
                        // A torn read that still parsed would most likely show
                        // up as losing the seed entry that is only ever added.
                        assert!(
                            index.entries.contains_key("seed@1"),
                            "an entry that is never removed vanished from a concurrent read"
                        );
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);

        let total: u32 = readers.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(
            total > 0,
            "the readers must have actually observed the index"
        );
    }

    /// Every other concurrency test here races `add` against `add`. Removals
    /// take the same lock and rewrite the same file, so a mixed workload is
    /// where an asymmetry would show up — e.g. a removal path that wrote the
    /// index outside the locked section. Each thread owns a disjoint key and
    /// adds then removes it, so the end state is exactly the untouched
    /// keep-alive entries regardless of interleaving.
    #[test]
    fn adds_and_removals_racing_on_one_index_leave_exactly_the_expected_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        seed(&root, "keep@1", "2026-01-01T00:00:00Z");
        seed(&root, "keep@2", "2026-01-01T00:00:00Z");

        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let key = format!("churn-{w}@1");
                    barrier.wait();
                    for _ in 0..10 {
                        with_locked_index(&root, |index| {
                            index
                                .entries
                                .insert(key.clone(), entry_fixture("2026-01-01T00:00:00Z"));
                            Ok::<_, CatalogError>(())
                        })
                        .unwrap();
                        with_locked_index(&root, |index| {
                            index.entries.remove(&key).expect(
                                "a key only this thread ever touches must still be present",
                            );
                            Ok::<_, CatalogError>(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let index = read_index(&root).expect("the index must still parse after mixed contention");
        let names: Vec<_> = index.entries.keys().cloned().collect();
        assert_eq!(
            names,
            vec!["keep@1".to_string(), "keep@2".to_string()],
            "churn keys must all be gone and the untouched entries must survive"
        );
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

    #[test]
    fn many_concurrent_writers_of_identical_bytes_never_corrupt_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let content = b"identical content raced by many concurrent writers";

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || write_blob(&root, content).unwrap())
            })
            .collect();

        let hashes: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            hashes.iter().all(|h| h == &hashes[0]),
            "every writer must compute and report the same hash: {hashes:?}"
        );

        let hex = hashes[0].strip_prefix("sha256:").unwrap();
        let blob_bytes = std::fs::read(root.join("blobs").join(hex)).unwrap();
        assert_eq!(
            blob_bytes, content,
            "the published blob must be exactly the input bytes, not truncated or corrupted by a racing writer"
        );
    }

    fn make_bundle(manifest_json: &[u8]) -> Vec<u8> {
        let mut bytes = b"CFBD".to_vec();
        bytes.extend_from_slice(&(manifest_json.len() as u64).to_le_bytes());
        bytes.extend_from_slice(manifest_json);
        bytes
    }

    #[test]
    fn reads_the_signature_field_out_of_a_valid_bundle_manifest() {
        let bundle = make_bundle(
            br#"{"nodes":[],"edges":[],"signature":"{path: text} -> {summary: text}"}"#,
        );
        let sig = read_bundle_signature(&bundle, "test.cfbundle").unwrap();
        assert_eq!(sig, "{path: text} -> {summary: text}");
    }

    #[test]
    fn a_manifest_len_exceeding_the_actual_bytes_is_uninspectable() {
        let mut bundle = make_bundle(br#"{"nodes":[],"edges":[],"signature":"x -> x"}"#);
        bundle.truncate(bundle.len() - 5); // manifest_len now overshoots what's left
        let err = read_bundle_signature(&bundle, "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => assert!(
                reason.contains("exceeds the file's actual length"),
                "{reason}"
            ),
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn invalid_manifest_json_is_uninspectable() {
        let bundle = make_bundle(b"not valid json at all");
        let err = read_bundle_signature(&bundle, "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => {
                assert!(reason.contains("not valid JSON"), "{reason}")
            }
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_missing_the_signature_field_is_uninspectable() {
        let bundle = make_bundle(br#"{"nodes":[],"edges":[]}"#);
        let err = read_bundle_signature(&bundle, "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => {
                assert!(reason.contains("no string field"), "{reason}")
            }
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn a_node_whose_offset_and_len_overflow_the_stage_bytes_is_uninspectable() {
        // No stage bytes follow the manifest at all here, so any non-zero
        // offset/len is already out of bounds — exactly the "internally
        // impossible node table" shape a hand-crafted or corrupt bundle
        // could smuggle past a check that only ever reads the `signature`
        // field.
        let bundle = make_bundle(
            br#"{"nodes":[{"name":"bad","kind":"block","resolved":null,
                 "signature":"json -> json","offset":99999,"len":99999}],
                 "signature":"json -> json"}"#,
        );
        let err = read_bundle_signature(&bundle, "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => {
                assert!(reason.contains("doesn't fit"), "{reason}")
            }
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn a_node_whose_offset_and_len_exactly_fit_the_stage_bytes_is_fine() {
        let mut bundle = make_bundle(
            br#"{"nodes":[{"name":"ok","kind":"block","resolved":null,
                 "signature":"json -> json","offset":0,"len":3}],
                 "signature":"json -> json"}"#,
        );
        bundle.extend_from_slice(b"abc");
        let sig = read_bundle_signature(&bundle, "test.cfbundle").unwrap();
        assert_eq!(sig, "json -> json");
    }

    #[test]
    fn an_overflowing_node_offset_plus_len_is_uninspectable_not_a_panic() {
        // Regression-shaped like the manifest_len overflow fix elsewhere in
        // this file: offset + len must not panic on overflow, it must
        // report a clean error.
        let bundle = make_bundle(
            format!(
                r#"{{"nodes":[{{"name":"bad","kind":"block","resolved":null,
                     "signature":"json -> json","offset":{},"len":10}}],
                     "signature":"json -> json"}}"#,
                u64::MAX
            )
            .as_bytes(),
        );
        let err = read_bundle_signature(&bundle, "test.cfbundle").unwrap_err();
        assert!(matches!(err, CatalogError::UninspectableArtifact { .. }));
    }

    #[test]
    fn an_overflowing_manifest_len_is_uninspectable_not_a_panic() {
        // manifest_len near u64::MAX must not panic when added to BUNDLE_HEADER_LEN —
        // regression test for the checked_add fix (a bare `+` here panics with
        // "attempt to add with overflow" in debug builds, turning a crafted
        // bundle file into a crash instead of a clean error).
        let mut bytes = b"CFBD".to_vec();
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        let err = read_bundle_signature(&bytes, "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => {
                assert!(
                    reason.contains("exceeds the file's actual length"),
                    "{reason}"
                )
            }
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn a_file_shorter_than_the_header_is_uninspectable() {
        let err = read_bundle_signature(b"CFBD", "test.cfbundle").unwrap_err();
        match err {
            CatalogError::UninspectableArtifact { reason, .. } => {
                assert!(
                    reason.contains("shorter than the bundle header"),
                    "{reason}"
                )
            }
            other => panic!("expected UninspectableArtifact, got {other:?}"),
        }
    }

    #[test]
    fn adding_a_wasm_block_with_no_cf_signature_export_caches_the_permissive_default_and_flags_it()
    {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm_path = wasm_dir.path().join("no_sig.wasm");
        std::fs::write(
            &wasm_path,
            wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap(),
        )
        .unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let outcome = catalog
            .add("no-sig@1", &wasm_path, &wasmtime::Engine::default())
            .expect("a block missing cf_signature is not an add-time error");

        assert_eq!(outcome.signature, "json -> json");
        assert!(
            outcome.is_permissive_default,
            "a block with no cf_signature export must be flagged, not silently accepted"
        );
    }

    #[test]
    fn adding_wasm_magic_bytes_with_an_invalid_module_body_is_uninspectable() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm_path = wasm_dir.path().join("broken.wasm");
        // Real wasm magic, garbage after it: passes the magic-byte sniff, fails
        // Module::new — the "recognised header, unreadable contents" case.
        std::fs::write(
            &wasm_path,
            b"\0asm\x01\x00\x00\x00garbage-not-a-real-module",
        )
        .unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let err = catalog
            .add("broken@1", &wasm_path, &wasmtime::Engine::default())
            .unwrap_err();
        assert!(
            matches!(err, CatalogError::UninspectableArtifact { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_cf_signature_export_that_exists_but_returns_unparseable_bytes_is_uninspectable_not_permissive(
    ) {
        // Distinct from both prior tests: the module instantiates fine and
        // cf_signature exists with the right callable shape (() -> u32) — this
        // is the "present but broken" case, which must NOT be folded into the
        // "absent" case's permissive-default fallback. The descriptor it returns
        // points at zeroed memory (no data segment): reading it back yields an
        // empty buffer, which fails to parse as a Signature — read_signature
        // returns Err, and that must surface as UninspectableArtifact.
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm_path = wasm_dir.path().join("broken_sig.wasm");
        std::fs::write(
            &wasm_path,
            wat::parse_str(
                r#"(module
                     (memory (export "memory") 1)
                     (func (export "cf_signature") (result i32) i32.const 0)
                   )"#,
            )
            .unwrap(),
        )
        .unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let err = catalog
            .add("broken-sig@1", &wasm_path, &wasmtime::Engine::default())
            .unwrap_err();
        assert!(
            matches!(err, CatalogError::UninspectableArtifact { .. }),
            "present-but-unparseable cf_signature must be a hard failure, not the permissive default: {err:?}"
        );
    }

    #[test]
    fn adding_a_bundle_reads_its_signature_from_the_manifest_never_instantiating_wasm() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let bundle_dir = tempfile::tempdir().unwrap();
        let bundle_path = bundle_dir.path().join("digest.cfbundle");
        std::fs::write(
            &bundle_path,
            make_bundle(
                br#"{"nodes":[],"edges":[],"signature":"{path: text} -> {summary: text}"}"#,
            ),
        )
        .unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let outcome = catalog
            .add("digest@1", &bundle_path, &wasmtime::Engine::default())
            .unwrap();

        assert_eq!(outcome.kind, ArtifactKind::Bundle);
        assert_eq!(outcome.signature, "{path: text} -> {summary: text}");
        assert!(!outcome.is_permissive_default);
    }

    #[test]
    fn adding_a_file_with_neither_magic_is_unrecognized_not_a_silent_guess() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let junk_dir = tempfile::tempdir().unwrap();
        let junk_path = junk_dir.path().join("junk.bin");
        std::fs::write(&junk_path, b"not a wasm or bundle").unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let err = catalog
            .add("junk@1", &junk_path, &wasmtime::Engine::default())
            .unwrap_err();
        assert!(
            matches!(err, CatalogError::UnrecognizedArtifact { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn re_adding_the_same_name_version_is_rejected() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm_path = wasm_dir.path().join("a.wasm");
        std::fs::write(
            &wasm_path,
            wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap(),
        )
        .unwrap();

        let catalog = Catalog::open(catalog_dir.path());
        let engine = wasmtime::Engine::default();
        catalog.add("dup@1", &wasm_path, &engine).unwrap();

        let err = catalog.add("dup@1", &wasm_path, &engine).unwrap_err();
        assert!(matches!(err, CatalogError::AlreadyExists { .. }), "{err:?}");
    }

    #[test]
    fn list_show_rm_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "a@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());

        assert_eq!(catalog.list().unwrap().len(), 1);
        let shown = catalog
            .show("a@1")
            .expect("just-seeded entry must be visible");
        assert_eq!(shown.signature, "json -> json");

        catalog.rm("a@1").unwrap();
        assert!(catalog.list().unwrap().is_empty());
    }

    #[test]
    fn showing_a_missing_entry_reports_not_found_with_a_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "summarize@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());

        let err = catalog.show("summarise@1").unwrap_err();
        let CatalogError::NotFound { did_you_mean, .. } = &err else {
            panic!("expected NotFound, got {err:?}")
        };
        assert_eq!(did_you_mean, &vec!["summarize@1".to_string()]);
    }

    /// Write a valid, signature-less wasm block whose bytes vary with
    /// `body_marker`, so two calls can produce artifacts that are both valid
    /// and genuinely different content.
    fn distinct_wasm(dir: &Path, name: &str, body_marker: u32) -> PathBuf {
        let path = dir.join(format!("{name}.wasm"));
        std::fs::write(
            &path,
            wat::parse_str(format!(
                r#"(module (memory (export "memory") 1) (func (export "marker") (result i32) i32.const {body_marker}))"#
            ))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn an_identifier_with_no_at_version_is_rejected_rather_than_catalogued_under_a_typo() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "block", 1);

        let err = Catalog::open(catalog_dir.path())
            .add("echo-summarize", &wasm, &wasmtime::Engine::default())
            .expect_err("dropping @version is a typo, not a name meaning itself");

        assert!(
            matches!(err, CatalogError::InvalidNameVersion { .. }),
            "{err:?}"
        );
        assert!(
            Catalog::open(catalog_dir.path()).list().unwrap().is_empty(),
            "a rejected identifier must not leave an entry behind"
        );
    }

    #[test]
    fn an_identifier_with_an_empty_name_or_version_is_rejected() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "block", 1);
        let catalog = Catalog::open(catalog_dir.path());
        let engine = wasmtime::Engine::default();

        for bad in ["@1", "name@", "", "   "] {
            let err = catalog
                .add(bad, &wasm, &engine)
                .expect_err("an empty name or version is not a name@version");
            assert!(
                matches!(err, CatalogError::InvalidNameVersion { .. }),
                "{bad:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn an_identifier_with_more_than_one_at_separator_is_rejected() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "block", 1);

        let err = Catalog::open(catalog_dir.path())
            .add("a@b@c", &wasm, &wasmtime::Engine::default())
            .expect_err("two '@' separators is not a name@version");
        assert!(
            matches!(err, CatalogError::InvalidNameVersion { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_identifier_containing_path_or_whitespace_characters_is_rejected() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "block", 1);
        let catalog = Catalog::open(catalog_dir.path());
        let engine = wasmtime::Engine::default();

        for bad in ["../../etc/passwd@1", "with space@1", "name@../../tmp/pwn"] {
            let err = catalog
                .add(bad, &wasm, &engine)
                .expect_err("{bad} must be rejected");
            assert!(
                matches!(err, CatalogError::InvalidNameVersion { .. }),
                "{bad:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn an_ordinary_name_at_version_still_catalogs() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "block", 1);

        Catalog::open(catalog_dir.path())
            .add(
                "echo-summarize@1.2.3-rc.1",
                &wasm,
                &wasmtime::Engine::default(),
            )
            .expect("letters, digits, '.', '-' and '_' are all legal");
    }

    /// Validation guards the *write* path only. An index that already holds a
    /// junk key (written before this check existed, or hand-edited) must stay
    /// removable, or the fix would strand entries nothing can clean up.
    #[test]
    fn a_pre_existing_junk_identifier_can_still_be_shown_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "no-at-sign", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());

        catalog
            .show("no-at-sign")
            .expect("an already-stored key must remain inspectable");
        catalog
            .rm("no-at-sign")
            .expect("an already-stored key must remain removable");
    }

    #[test]
    fn re_adding_a_removed_version_with_the_same_bytes_is_allowed() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let wasm = distinct_wasm(wasm_dir.path(), "same", 7);
        let catalog = Catalog::open(catalog_dir.path());
        let engine = wasmtime::Engine::default();

        catalog.add("thing@1", &wasm, &engine).unwrap();
        catalog.rm("thing@1").unwrap();
        catalog
            .add("thing@1", &wasm, &engine)
            .expect("re-adding identical bytes is an undo of the rm, not a rewrite of history");

        assert_eq!(catalog.list().unwrap().len(), 1);
    }

    /// The hazard the immutability promise exists to prevent: a name@version
    /// that someone already depends on silently coming to mean different
    /// content. Deleting the entry first must not launder that.
    #[test]
    fn re_adding_a_removed_version_with_different_bytes_is_rejected() {
        let catalog_dir = tempfile::tempdir().unwrap();
        let wasm_dir = tempfile::tempdir().unwrap();
        let original = distinct_wasm(wasm_dir.path(), "original", 1);
        let replacement = distinct_wasm(wasm_dir.path(), "replacement", 2);
        let catalog = Catalog::open(catalog_dir.path());
        let engine = wasmtime::Engine::default();

        catalog.add("thing@1", &original, &engine).unwrap();
        catalog.rm("thing@1").unwrap();

        let err = catalog
            .add("thing@1", &replacement, &engine)
            .expect_err("rm must not be a way to republish a version with new content");
        let CatalogError::RetiredWithDifferentContent {
            name_version,
            previous_hash,
            new_hash,
        } = &err
        else {
            panic!("expected RetiredWithDifferentContent, got {err:?}")
        };
        assert_eq!(name_version, "thing@1");
        assert_ne!(previous_hash, new_hash);
        assert!(
            catalog.list().unwrap().is_empty(),
            "the reject must not add"
        );
    }

    /// An index written before retirement tracking existed has no `retired`
    /// field at all. It must still load as a normal, non-corrupt catalog
    /// rather than tripping the version check.
    #[test]
    fn an_index_written_without_the_retired_field_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            br#"{"version":1,"entries":{"old@1":{"hash":"sha256:ab","kind":"block","signature":"json -> json","created_at":"2026-01-01T00:00:00Z"}}}"#,
        )
        .unwrap();

        let index = read_index(dir.path()).expect("an index predating `retired` is not corrupt");
        assert!(index.entries.contains_key("old@1"));
        assert!(index.retired.is_empty());
    }

    #[test]
    fn removing_a_missing_entry_is_not_found_not_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path());
        let err = catalog.rm("nothing@1").unwrap_err();
        assert!(matches!(err, CatalogError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn removing_an_entry_leaves_its_blob_on_disk_v1_has_no_garbage_collection() {
        let dir = tempfile::tempdir().unwrap();
        let hash = write_blob(dir.path(), b"some block bytes").unwrap();
        let hex = hash.strip_prefix("sha256:").unwrap();
        with_locked_index(dir.path(), |index| {
            index.entries.insert(
                "a@1".to_string(),
                Entry {
                    hash: hash.clone(),
                    kind: ArtifactKind::Block,
                    signature: "json -> json".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                },
            );
            Ok::<_, CatalogError>(())
        })
        .unwrap();

        let catalog = Catalog::open(dir.path());
        catalog.rm("a@1").unwrap();

        assert!(
            matches!(catalog.show("a@1"), Err(CatalogError::NotFound { .. })),
            "rm must actually remove the index entry, not silently no-op"
        );
        assert!(
            dir.path().join("blobs").join(hex).exists(),
            "rm is index-only; the blob must remain"
        );
    }

    #[test]
    fn list_returns_multiple_entries_sorted_by_name_at_version() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "b@1", "2026-01-01T00:00:00Z");
        seed(dir.path(), "a@1", "2026-01-01T00:00:00Z");
        seed(dir.path(), "c@1", "2026-01-01T00:00:00Z");

        let catalog = Catalog::open(dir.path());
        let names: Vec<String> = catalog
            .list()
            .unwrap()
            .into_iter()
            .map(|(name_version, _)| name_version)
            .collect();

        assert_eq!(
            names,
            vec!["a@1".to_string(), "b@1".to_string(), "c@1".to_string()]
        );
    }

    #[test]
    fn resolve_a_dot_wasm_suffix_is_direct_even_if_the_file_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path());
        let resolved = catalog
            .resolve("/nonexistent/block.wasm", ResolutionContext::Interactive)
            .unwrap();
        assert!(matches!(resolved, Resolved::Direct(_)));
    }

    #[test]
    fn resolve_a_dot_cfbundle_suffix_is_direct_even_if_the_file_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path());
        let resolved = catalog
            .resolve(
                "/nonexistent/bundle.cfbundle",
                ResolutionContext::Interactive,
            )
            .unwrap();
        assert!(matches!(resolved, Resolved::Direct(_)));
    }

    #[test]
    fn resolve_an_existing_filesystem_path_is_direct_no_catalog_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let real_file = tempfile::NamedTempFile::new().unwrap();
        let catalog = Catalog::open(dir.path());
        let resolved = catalog
            .resolve(
                real_file.path().to_str().unwrap(),
                ResolutionContext::Interactive,
            )
            .unwrap();
        assert!(matches!(resolved, Resolved::Direct(_)));
    }

    #[test]
    fn resolve_exact_name_at_version_hits_case_sensitively() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "summarize@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());

        assert!(catalog
            .resolve("summarize@1", ResolutionContext::Interactive)
            .is_ok());

        let err = catalog
            .resolve("Summarize@1", ResolutionContext::Interactive)
            .unwrap_err();
        let CatalogError::NotFound { did_you_mean, .. } = &err else {
            panic!("expected NotFound (case-sensitive miss), got {err:?}")
        };
        assert!(
            did_you_mean.contains(&"summarize@1".to_string()),
            "case-sensitivity rejects the hit, but edit distance 1 should still suggest it: {did_you_mean:?}"
        );
    }

    #[test]
    fn resolve_unqualified_name_picks_the_latest_by_created_at() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "a@1", "2026-01-01T00:00:00Z");
        seed(dir.path(), "a@2", "2026-06-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());

        let resolved = catalog
            .resolve("a", ResolutionContext::Interactive)
            .unwrap();
        let Resolved::Cataloged { name_version, .. } = resolved else {
            panic!("expected a cataloged resolution")
        };
        assert_eq!(name_version, "a@2");
    }

    #[test]
    fn resolve_unqualified_name_is_legal_from_an_interactive_context() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "a@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());
        assert!(catalog.resolve("a", ResolutionContext::Interactive).is_ok());
    }

    #[test]
    fn resolve_unqualified_name_is_rejected_in_a_durable_context() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "a@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());
        let err = catalog
            .resolve("a", ResolutionContext::Durable)
            .unwrap_err();
        assert!(
            matches!(err, CatalogError::UnqualifiedName { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_not_found_suggests_a_close_typo() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "summarize@1", "2026-01-01T00:00:00Z");
        let catalog = Catalog::open(dir.path());
        let err = catalog
            .resolve("summarise@1", ResolutionContext::Interactive)
            .unwrap_err();
        let CatalogError::NotFound { did_you_mean, .. } = &err else {
            panic!("expected NotFound, got {err:?}")
        };
        assert_eq!(did_you_mean, &vec!["summarize@1".to_string()]);
    }

    #[test]
    fn read_blob_returns_what_add_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path());
        let engine = wasmtime::Engine::default();
        let wasm = wat::parse_str("(module)").unwrap();
        let path = dir.path().join("m.wasm");
        std::fs::write(&path, &wasm).unwrap();

        let outcome = catalog.add("m@1", &path, &engine).unwrap();
        let entry = catalog.show("m@1").unwrap();

        let bytes = catalog.read_blob(&entry).unwrap();
        assert_eq!(bytes, wasm);
        assert_eq!(outcome.name_version, "m@1");
    }

    #[test]
    fn read_blob_on_a_hand_edited_missing_hash_errors_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path());
        let fake = Entry {
            hash: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            ..entry_fixture("2026-01-01T00:00:00Z")
        };
        let err = catalog.read_blob(&fake).unwrap_err();
        match err {
            CatalogError::Io(ref io_err) => {
                assert_eq!(
                    io_err.kind(),
                    std::io::ErrorKind::NotFound,
                    "a well-formed hash with no matching blob file must surface as a plain \
                     not-found I/O error: {err:?}"
                );
            }
            other => {
                panic!("a well-formed but absent hash must be a plain Io(NotFound), not {other:?}")
            }
        }
    }

    #[test]
    fn read_blob_rejects_a_path_traversal_hash_instead_of_touching_the_filesystem() {
        // A hand-edited (or maliciously crafted) index.json is never
        // format-validated on read anywhere else in this module — read_blob
        // is the last line of defense before a hash string becomes a
        // filesystem path. A well-formed sha256 digest is always exactly 64
        // lowercase hex digits (see write_blob's `format!("{:x}", ...)`), so
        // anything else — especially `../` traversal or an absolute path —
        // must be rejected before Path::join ever sees it.
        let dir = tempfile::tempdir().unwrap();
        // Plant a marker file outside blobs/ that a traversal would reach if
        // the guard were missing.
        std::fs::write(dir.path().join("outside.txt"), b"do not leak this").unwrap();

        let catalog = Catalog::open(dir.path());
        let traversal = Entry {
            hash: "sha256:../outside.txt".to_string(),
            ..entry_fixture("2026-01-01T00:00:00Z")
        };
        let err = catalog.read_blob(&traversal).unwrap_err();
        assert!(
            matches!(err, CatalogError::MalformedHash { .. }),
            "a path-traversal hash must be rejected as MalformedHash before any path is \
             constructed, got {err:?}"
        );

        let absolute = Entry {
            hash: "sha256:/etc/passwd".to_string(),
            ..entry_fixture("2026-01-01T00:00:00Z")
        };
        let err = catalog.read_blob(&absolute).unwrap_err();
        assert!(
            matches!(err, CatalogError::MalformedHash { .. }),
            "an absolute-path-like hash must be rejected as MalformedHash before any path is \
             constructed, got {err:?}"
        );
    }

    #[test]
    fn a_simple_lowercase_name_is_valid() {
        assert!(validate_block_name("my-block").is_ok());
    }

    #[test]
    fn a_name_with_a_dot_is_rejected() {
        let err = validate_block_name("my.block").unwrap_err();
        assert!(err.to_string().contains('.'), "{err}");
    }

    #[test]
    fn a_name_starting_with_a_digit_is_rejected() {
        assert!(validate_block_name("1block").is_err());
    }

    #[test]
    fn a_windows_reserved_device_name_is_rejected_case_insensitively() {
        for bad in ["con", "CON", "Con", "aux", "nul", "com1", "lpt9"] {
            assert!(
                validate_block_name(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn a_name_that_only_resembles_a_reserved_name_is_accepted() {
        assert!(validate_block_name("console").is_ok());
        assert!(validate_block_name("commander").is_ok());
    }
}
