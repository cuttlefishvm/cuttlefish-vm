//! Shared helpers for cuttlefish's daemon-facing tests (`daemon.rs`).
//!
//! Mirrors the equivalent helpers in `crates/cuttlefishd/tests/api.rs`: these
//! tests spawn a *real* `cuttlefishd` process and talk to it over a real
//! transport, nothing mocked.

use std::path::{Path, PathBuf};

/// Build `cuttlefishd` once per test binary and return its compiled path.
///
/// `env!("CARGO_BIN_EXE_cuttlefishd")` doesn't work here — that's only
/// populated for a binary belonging to the *same* package as the test
/// binary, not a sibling workspace crate's. Build and locate manually
/// instead, same pattern `crates/cuttlefishd/tests/api.rs`'s own
/// `example_block()` helper already uses for a sibling crate's artifact.
pub fn cuttlefishd_binary() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "cuttlefishd"])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building cuttlefishd failed");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        root.join("target/debug/cuttlefishd")
    })
    .clone()
}

/// Build the example block once per test binary, the same fixture
/// `crates/cuttlefishd/tests/api.rs`'s own `example_block()` builds — reused
/// here rather than duplicated as a distinct wasm source, since these tests
/// only need *some* real, loadable block to satisfy the daemon's startup
/// typecheck, not any particular behavior from it.
pub fn example_block() -> Vec<u8> {
    static WASM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

    WASM.get_or_init(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "cf-block-echo-summarize",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .status()
            .expect("cargo build failed to start");
        assert!(status.success(), "building the example block failed");

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = root.join("target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("reading {}: {e}", wasm.display()))
    })
    .clone()
}

/// Point `$CUTTLEFISH_HOME` at one tempdir shared by every test in this
/// binary, so a spawned `cuttlefishd`'s job ledger and catalog never touch a
/// developer's real home directory. `std::process::Command::spawn` inherits
/// the calling process's environment by default, so setting this here is
/// enough for every `spawn_daemon` call afterward to pick it up — no need to
/// thread it through as an explicit argument. Mirrors
/// `crates/cuttlefishd/tests/api.rs`'s own `ensure_test_cuttlefish_home`.
///
/// A `OnceLock` plus its own internal `Once`-style init (via `get_or_init`,
/// which itself synchronizes concurrent callers) means every concurrently
/// running `#[tokio::test]` in this binary computes and sets the exact same
/// value at most once, rather than racing distinct `std::env::set_var` calls
/// against each other.
pub fn ensure_test_cuttlefish_home() {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CUTTLEFISH_HOME", dir.path());
        dir
    });
}

/// Spawn `cuttlefishd` against `spec_path`/`endpoint`, wait (briefly,
/// bounded) for it to actually accept connections before returning, and
/// return the child so the caller can kill it in a cleanup step.
///
/// Panics (after killing the child) if the daemon never becomes ready, so
/// callers only need to handle the happy path.
pub async fn spawn_daemon(spec_path: &Path, endpoint: &Path) -> std::process::Child {
    let mut child = std::process::Command::new(cuttlefishd_binary())
        .arg(spec_path)
        .arg(endpoint)
        .spawn()
        .expect("cuttlefishd failed to start");

    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(endpoint);
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(endpoint);
    let client = builder.build().unwrap();

    let mut ready = false;
    for _ in 0..50 {
        if client.get("http://localhost/specs").send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if !ready {
        let _ = child.kill();
        panic!(
            "cuttlefishd did not become ready in time on {}",
            endpoint.display()
        );
    }
    child
}
