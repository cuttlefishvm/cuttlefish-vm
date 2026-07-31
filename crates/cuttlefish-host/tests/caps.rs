//! Capability enforcement is the security boundary, so these tests are written
//! adversarially: each one is an escape a naive implementation would allow.
//!
//! The naive implementation is a string prefix check (`path.starts_with(root)`),
//! and every test here except the happy path defeats it.

use cuttlefish_host::caps::Capabilities;
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
