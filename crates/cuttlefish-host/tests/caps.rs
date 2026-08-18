//! Capability enforcement is the security boundary, so these tests are written
//! adversarially: each one is an escape a naive implementation would allow.
//!
//! The naive implementation is a string prefix check (`path.starts_with(root)`),
//! and every test here except the happy path defeats it.

use cuttlefish_host::caps::{Capabilities, ReadDenial};
use std::fs;

#[test]
fn allows_a_read_under_a_granted_root() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ok.txt");
    fs::write(&file, "hi").unwrap();

    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    assert!(caps.allows_read(&file));
}

#[test]
fn allows_a_read_nested_deeper_than_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a/b/c");
    fs::create_dir_all(&nested).unwrap();
    let file = nested.join("deep.txt");
    fs::write(&file, "hi").unwrap();

    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    assert!(caps.allows_read(&file));
}

#[test]
fn denies_a_read_outside_every_granted_root() {
    let granted = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    fs::write(&secret, "nope").unwrap();

    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);
    assert!(!caps.allows_read(&secret));
}

#[test]
fn denies_traversal_out_of_a_granted_root() {
    // The attack a string prefix check waves straight through:
    // "/granted/inner/../../outside.txt" starts with "/granted/inner".
    let root = tempfile::tempdir().unwrap();
    let inner = root.path().join("inner");
    fs::create_dir(&inner).unwrap();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "nope").unwrap();

    let caps = Capabilities::new(vec![inner.clone()]);
    assert!(!caps.allows_read(&inner.join("../outside.txt")));
}

#[cfg(unix)]
#[test]
fn denies_a_symlink_pointing_out_of_a_granted_root() {
    // The second attack a prefix check misses: the path really is under the
    // granted root, but the file it names is not.
    let granted = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    fs::write(&secret, "nope").unwrap();

    let link = granted.path().join("innocent.txt");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);
    assert!(
        !caps.allows_read(&link),
        "a symlink escaping the granted root must be denied"
    );
}

#[test]
fn denies_everything_when_no_capability_is_granted() {
    // Deny-by-default: the empty grant set permits nothing, rather than
    // vacuously permitting everything.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, "x").unwrap();

    assert!(!Capabilities::default().allows_read(&file));
}

#[test]
fn denies_a_path_that_does_not_exist() {
    // A nonexistent path cannot be proven to be inside the grant, so it is
    // refused. Failing closed here also removes a race: the check cannot pass
    // for a file that is created afterwards.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    assert!(!caps.allows_read(&dir.path().join("not-created-yet.txt")));
}

#[test]
fn denies_when_a_granted_root_does_not_exist() {
    // A spec naming a root that is not there must not accidentally widen access.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, "x").unwrap();

    let caps = Capabilities::new(vec!["/definitely/not/a/real/path".into()]);
    assert!(!caps.allows_read(&file));
}

#[test]
fn a_sibling_directory_sharing_a_name_prefix_is_denied() {
    // "/tmp/data-secret" starts with "/tmp/data" as a *string*, but is not
    // inside it as a *path*.
    let base = tempfile::tempdir().unwrap();
    let granted = base.path().join("data");
    let sibling = base.path().join("data-secret");
    fs::create_dir(&granted).unwrap();
    fs::create_dir(&sibling).unwrap();

    let file = sibling.join("f.txt");
    fs::write(&file, "nope").unwrap();

    let caps = Capabilities::new(vec![granted]);
    assert!(
        !caps.allows_read(&file),
        "a name-prefix sibling must not be treated as nested"
    );
}

#[test]
fn any_one_of_several_granted_roots_suffices() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let file = b.path().join("f.txt");
    fs::write(&file, "hi").unwrap();

    let caps = Capabilities::new(vec![a.path().to_path_buf(), b.path().to_path_buf()]);
    assert!(caps.allows_read(&file));
}

/// A relative path is resolved against the process working directory, not
/// against the granted root.
///
/// This is the rule that is easy to assume backwards: granting `./docs` and
/// then asking to read `docs/note.txt` only works if the *caller's* working
/// directory is the one the grant was written relative to. Getting it wrong
/// looks exactly like a capability bug — a denial on a file that plainly
/// exists inside the granted directory — so it is pinned here rather than
/// rediscovered.
#[test]
fn a_relative_path_is_resolved_against_the_working_directory_not_the_root() {
    let cwd = std::env::current_dir().unwrap();
    // Inside the working directory, so a relative spelling can reach it. Self
    // cleaning, so it does not leave anything behind in the crate.
    let here = tempfile::tempdir_in(&cwd).unwrap();
    let leaf = here.path().file_name().unwrap();

    let file = here.path().join("f.txt");
    fs::write(&file, "hi").unwrap();

    let relative = std::path::Path::new(leaf).join("f.txt");
    assert!(relative.is_relative(), "the point of this test");

    let caps = Capabilities::new(vec![here.path().to_path_buf()]);
    assert!(
        caps.allows_read(&relative),
        "a relative path that resolves inside the grant must be allowed"
    );

    // The same spelling, against a grant somewhere else entirely, is denied —
    // the resolution is real, not a string comparison that happened to match.
    let elsewhere = tempfile::tempdir().unwrap();
    let other_caps = Capabilities::new(vec![elsewhere.path().to_path_buf()]);
    assert!(
        !other_caps.allows_read(&relative),
        "the same relative spelling must not be allowed by an unrelated grant"
    );
}

#[test]
fn a_fetch_grant_covers_its_prefix_and_nothing_else() {
    let caps = Capabilities::new(vec![]).with_fetch(vec!["https://www.cms.gov/medicare/".into()]);

    assert!(caps.allows_fetch("https://www.cms.gov/medicare/transmittals?page=2"));
    // A sibling path under the same host is not granted: the prefix is the
    // rule, so an author can predict it from their own capability line.
    assert!(!caps.allows_fetch("https://www.cms.gov/medicaid/other"));
    // The scheme is part of the prefix — http is not https.
    assert!(!caps.allows_fetch("http://www.cms.gov/medicare/x"));
    // Another host entirely, however similar.
    assert!(!caps.allows_fetch("https://www.cms.gov.evil.test/medicare/x"));
}

#[test]
fn a_url_cannot_climb_out_of_its_granted_prefix() {
    // Without this, a granted subtree reaches anything on the host, which
    // makes the capability line a description of nothing.
    let caps = Capabilities::new(vec![]).with_fetch(vec!["https://x.test/docs/".into()]);
    assert!(caps.allows_fetch("https://x.test/docs/a.pdf"));
    assert!(!caps.allows_fetch("https://x.test/docs/../secrets/a.pdf"));
}

#[test]
fn no_fetch_grant_means_no_fetching() {
    // The default. A spec that says nothing about the network gets none.
    let caps = Capabilities::new(vec!["/tmp".into()]);
    assert!(!caps.allows_fetch("https://anything.test/"));
    assert!(caps.fetch_prefixes().is_empty());
}

#[test]
fn a_missing_file_inside_a_grant_is_reported_as_missing_not_forbidden() {
    // The friction this removes: a manifest naming a file that moved failed
    // with "read not permitted", sending the reader to re-check their
    // `capabilities` line when the grant was never the problem.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);

    let absent = dir.path().join("gone.txt");
    assert_eq!(caps.read_denial(&absent), Some(ReadDenial::Missing));

    // A file that is there is not denied at all.
    let present = dir.path().join("here.txt");
    std::fs::write(&present, b"x").unwrap();
    assert_eq!(caps.read_denial(&present), None);
}

#[test]
fn absence_outside_the_grant_is_not_reported() {
    // The security property: distinguishing "absent" from "forbidden" for a
    // path the job was never granted is exactly the probe a capability list
    // exists to prevent. Both must come back as the same undifferentiated
    // refusal.
    let granted = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);

    let real = elsewhere.path().join("real.txt");
    std::fs::write(&real, b"secret").unwrap();
    let imaginary = elsewhere.path().join("imaginary.txt");

    assert_eq!(caps.read_denial(&real), Some(ReadDenial::NotGranted));
    assert_eq!(
        caps.read_denial(&imaginary),
        Some(ReadDenial::NotGranted),
        "a caller must not be able to tell an absent file from a forbidden one \
         outside the grant — that difference is an existence oracle"
    );
}

#[test]
fn a_missing_directory_inside_a_grant_still_reports_missing() {
    // The nearest *existing* ancestor is what decides, so a path several
    // levels below a granted root that does not exist yet is still diagnosed
    // as a wrong path rather than a wrong grant.
    let dir = tempfile::tempdir().unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    let deep = dir.path().join("a/b/c/report.pdf");
    assert_eq!(caps.read_denial(&deep), Some(ReadDenial::Missing));
}

#[test]
fn traversal_out_of_a_grant_is_still_refused_outright() {
    // `..` must not become a way to ask about files elsewhere: the resolved
    // location decides, and a path resolving outside the grant is refused
    // without saying whether anything is there.
    let granted = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("secret.txt"), b"s").unwrap();
    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);

    let escape = granted
        .path()
        .join("..")
        .join(elsewhere.path().file_name().unwrap())
        .join("secret.txt");
    assert_eq!(caps.read_denial(&escape), Some(ReadDenial::NotGranted));
}

#[test]
fn a_symlink_inside_a_grant_pointing_out_is_refused_not_called_missing() {
    // It is right there — the refusal is about where it resolves. Reporting
    // "no such file" would send somebody hunting for a typo in a path they
    // spelled correctly, and it is the one case where the nearest existing
    // ancestor is inside the grant while the target is not.
    let granted = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let secret = elsewhere.path().join("secret.txt");
    fs::write(&secret, b"s").unwrap();

    let link = granted.path().join("link.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&secret, &link).unwrap();

    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);
    assert_eq!(caps.read_denial(&link), Some(ReadDenial::NotGranted));
}

#[test]
fn a_dangling_symlink_inside_a_grant_is_not_called_missing_either() {
    // `exists()` follows the link and would report this as absent, which
    // would then be diagnosed as a wrong path. The link is not absent; its
    // target is, and that is a different sentence.
    let granted = tempfile::tempdir().unwrap();
    let link = granted.path().join("dangling.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(granted.path().join("nothing-here"), &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(granted.path().join("nothing-here"), &link).unwrap();

    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);
    assert_eq!(caps.read_denial(&link), Some(ReadDenial::NotGranted));
}
