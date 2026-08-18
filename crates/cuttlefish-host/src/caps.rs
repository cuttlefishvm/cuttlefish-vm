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
    fetch_prefixes: Vec<String>,
}

impl Capabilities {
    /// Grant read access beneath each of `read_roots`, and nothing else.
    pub fn new(read_roots: Vec<PathBuf>) -> Self {
        Self {
            read_roots,
            fetch_prefixes: Vec::new(),
        }
    }

    /// Also grant fetching any URL beginning with one of `fetch_prefixes`.
    pub fn with_fetch(mut self, fetch_prefixes: Vec<String>) -> Self {
        self.fetch_prefixes = fetch_prefixes;
        self
    }

    /// Whether `url` may be fetched.
    ///
    /// Prefix matching on the URL as written, deliberately: it is the rule a
    /// spec author can predict from reading their own capability line. No
    /// normalisation, no host-only matching — `Fetch "https://x.org/docs/"`
    /// grants that subtree and not `https://x.org/other`, and not
    /// `http://` either, since the scheme is part of the prefix.
    ///
    /// The one thing checked beyond the prefix is that the URL cannot climb
    /// out with `..`, which would otherwise let a granted prefix reach
    /// anywhere on the host.
    pub fn allows_fetch(&self, url: &str) -> bool {
        if url.contains("..") {
            return false;
        }
        self.fetch_prefixes.iter().any(|p| url.starts_with(p))
    }

    /// The granted fetch prefixes, as configured.
    pub fn fetch_prefixes(&self) -> &[String] {
        &self.fetch_prefixes
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
