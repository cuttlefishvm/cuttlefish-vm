//! What a job is permitted to reach.
//!
//! This is the security boundary. The compile-time check in `cuttlefish-core`
//! exists to give spec authors good error messages; the check *here* is the one
//! a malicious or malfunctioning block actually runs into, and it fails closed.

use std::path::{Path, PathBuf};

/// The capabilities a spec grants a job.
///
/// v1 has exactly one kind, filesystem reads under named roots. An empty set
/// grants nothing — deny-by-default is the whole posture, so "no capabilities
/// declared" must never read as "unrestricted".
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    read_roots: Vec<PathBuf>,
}

impl Capabilities {
    /// Grant read access beneath each of `read_roots`.
    pub fn new(read_roots: Vec<PathBuf>) -> Self {
        Self { read_roots }
    }

    /// Whether `path` may be read.
    ///
    /// Both sides are canonicalized before comparison, and that is the entire
    /// substance of this function. The tempting implementation —
    /// `path.starts_with(root)` on the raw strings — admits two escapes:
    ///
    /// - **Traversal.** `/granted/inner/../../secret` has `/granted/inner` as a
    ///   string prefix while naming a file outside it.
    /// - **Symlinks.** A path genuinely under the granted root can name a file
    ///   anywhere on the system.
    ///
    /// `canonicalize` resolves `..` and follows symlinks, so the comparison is
    /// between the real locations rather than the spellings. Comparing with
    /// `Path::starts_with` rather than string prefixes additionally means
    /// `/data-secret` is not treated as nested inside `/data`.
    ///
    /// A path that cannot be canonicalized — because it does not exist — is
    /// denied. That is deliberate on two counts: a nonexistent path cannot be
    /// *proven* inside the grant, and refusing it here means the decision cannot
    /// be made against a file that only appears afterwards.
    ///
    /// Note the residual limitation: this resolves the path once, and the caller
    /// opens it separately. A sufficiently determined attacker who can swap a
    /// symlink between those two steps still has a window. Closing it properly
    /// needs the caller to pass an already-open descriptor; see the transport
    /// discussion in the `cuttlefishd` crate docs.
    pub fn allows_read(&self, path: &Path) -> bool {
        let Ok(target) = path.canonicalize() else {
            return false;
        };
        self.read_roots.iter().any(|root| {
            root.canonicalize()
                .map(|root| target.starts_with(root))
                .unwrap_or(false)
        })
    }

    /// The granted read roots, as configured.
    pub fn read_roots(&self) -> &[PathBuf] {
        &self.read_roots
    }
}
