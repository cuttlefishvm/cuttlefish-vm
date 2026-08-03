//! Packaging a [`crate::pipeline::Checked`] pipeline into a `.cfbundle`.
//!
//! Container: `catalog::BUNDLE_MAGIC` (`b"CFBD"`), then `manifest_len: u64`
//! little-endian, then the JSON manifest, then the concatenated stage
//! bytes, with offsets recorded per node in the manifest. `catalog.rs`'s
//! `read_bundle_signature` is the read side of this exact format; the magic
//! bytes and header length are shared constants (`BUNDLE_MAGIC`,
//! `BUNDLE_HEADER_LEN`) rather than independently-defined literals, so the
//! two sides cannot silently drift apart.
//!
//! No timestamps or absolute paths anywhere in the output: that's what
//! makes two builds of the same spec against the same catalog state
//! byte-identical, so re-cataloging a rebuild never produces a spurious new
//! hash for "the same" pipeline.

use crate::catalog::{ArtifactKind, BUNDLE_HEADER_LEN, BUNDLE_MAGIC};
use crate::pipeline::Checked;
use serde::Serialize;

#[derive(Serialize)]
struct Manifest {
    manifest_version: u32,
    signature: String,
    nodes: Vec<Node>,
}

#[derive(Serialize)]
struct Node {
    name: String,
    kind: ArtifactKind,
    resolved: Option<String>,
    signature: String,
    /// Where this node's bytes start, relative to the *first byte after the
    /// manifest* — not an absolute offset into the `.cfbundle` file. A
    /// reader must compute `BUNDLE_HEADER_LEN + manifest_len + offset` to
    /// find the real file position. Nodes must always be located this way,
    /// never by scanning for `BUNDLE_MAGIC`: a node whose bytes are
    /// themselves a nested `.cfbundle` starts with that exact magic, so
    /// scanning for it would misidentify the inner bundle's own header as a
    /// fresh top-level one.
    offset: u64,
    len: u64,
}

/// Serialize a checked pipeline into `.cfbundle` bytes.
pub fn build(checked: &Checked) -> Vec<u8> {
    let mut offset = 0u64;
    let nodes: Vec<Node> = checked
        .stages()
        .iter()
        .map(|s| {
            let len = s.module_bytes.len() as u64;
            let node = Node {
                name: s.name.clone(),
                kind: s.kind,
                resolved: s.resolved.clone(),
                signature: s.signature.to_string(),
                offset,
                len,
            };
            offset += len;
            node
        })
        .collect();

    let manifest = Manifest {
        manifest_version: 1,
        signature: format!("{} -> {}", checked.input(), checked.output()),
        nodes,
    };
    let manifest_bytes = serde_json::to_vec(&manifest)
        .expect("Manifest always serializes: no non-finite floats, no non-string map keys");

    let mut out = Vec::with_capacity(BUNDLE_HEADER_LEN + manifest_bytes.len() + offset as usize);
    out.extend_from_slice(BUNDLE_MAGIC);
    out.extend_from_slice(&(manifest_bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&manifest_bytes);
    for stage in checked.stages() {
        out.extend_from_slice(&stage.module_bytes);
    }
    out
}
