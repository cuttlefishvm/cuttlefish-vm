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

    /// Why a read was refused, when it was.
    ///
    /// [`Self::allows_read`] answers yes-or-no, which is the right shape for
    /// the decision and the wrong shape for the message. Two very different
    /// situations reach it as `false`: a path the spec never granted, and a
    /// granted path that simply is not there. Reporting both as "read not
    /// permitted" sends somebody to re-read their `capabilities` line when
    /// what they actually have is a typo in a filename or a manifest listing
    /// a file that was moved.
    ///
    /// Saying "does not exist" is only safe where the job could have found
    /// out anyway. So it is said only when the nearest *existing* ancestor of
    /// the path is itself inside a granted root — a directory the job may
    /// already open and read. For anything else the answer stays the
    /// undifferentiated refusal, because distinguishing "absent" from
    /// "forbidden" outside the grant is exactly the probe a capability list
    /// exists to prevent.
    pub fn read_denial(&self, path: &Path) -> Option<ReadDenial> {
        if self.allows_read(path) {
            return None;
        }

        // Something is genuinely there — a symlink inside the grant pointing
        // out of it, say. It was refused because of where it *resolves*, not
        // because it is absent, and calling that "no such file" would send
        // the reader looking for a typo in a path that is spelled correctly.
        // `symlink_metadata` rather than `exists`, which follows the link and
        // would report the escape as absent whenever the target is.
        if path.symlink_metadata().is_ok() {
            return Some(ReadDenial::NotGranted);
        }

        // The nearest ancestor that exists. `canonicalize` fails on the whole
        // path when any component is missing, so walk up until it succeeds.
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            if let Ok(real) = dir.canonicalize() {
                let inside = self.read_roots.iter().any(|root| {
                    root.canonicalize()
                        .map(|root| real.starts_with(root))
                        .unwrap_or(false)
                });
                return Some(if inside {
                    ReadDenial::Missing
                } else {
                    ReadDenial::NotGranted
                });
            }
            ancestor = dir.parent();
        }
        Some(ReadDenial::NotGranted)
    }

    /// The granted read roots, as configured.
    pub fn read_roots(&self) -> &[PathBuf] {
        &self.read_roots
    }
}

/// Why [`Capabilities::allows_read`] said no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadDenial {
    /// The path is not inside any granted root — or is somewhere the job may
    /// not learn anything about, including whether it exists.
    NotGranted,
    /// The path would be readable, but there is nothing there. Only reported
    /// where the job could have discovered that itself; see
    /// [`Capabilities::read_denial`].
    Missing,
}
