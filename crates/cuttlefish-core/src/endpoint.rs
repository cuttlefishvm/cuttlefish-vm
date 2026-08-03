//! Where the daemon listens, and the client connects.
//!
//! This lives in `cuttlefish-core` because it is the one fact the daemon and
//! the CLI must agree on exactly, and they share no other crate — the CLI does
//! not depend on `cuttlefishd` (that would pull axum and the whole server
//! stack into a client). Restating the default in both places is precisely the
//! kind of thing that drifts.

use std::path::PathBuf;

/// The daemon's default endpoint for this platform.
///
/// Two different kinds of name wearing one type. On unix this is a filesystem
/// path and the socket is a real file; on Windows it is a named-pipe name,
/// which lives in the pipe namespace and is not a file at all. `PathBuf`
/// carries both because that is what the OS APIs take — do not infer that a
/// pipe name has a parent directory to create, or that it can be `remove_file`d.
pub fn default_endpoint() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/cuttlefish.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\cuttlefish")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon and the client each call this independently; if it ever
    /// returned something empty or relative, they would still agree with each
    /// other while both failing to connect.
    #[test]
    fn the_default_endpoint_is_absolute_and_non_empty() {
        let endpoint = default_endpoint();
        assert!(!endpoint.as_os_str().is_empty());

        let shown = endpoint.display().to_string();
        if cfg!(windows) {
            assert!(
                shown.starts_with(r"\\.\pipe\"),
                "a Windows endpoint must name the pipe namespace: {shown}"
            );
        } else {
            assert!(
                endpoint.is_absolute(),
                "a unix endpoint must not depend on the working directory: {shown}"
            );
        }
    }
}
