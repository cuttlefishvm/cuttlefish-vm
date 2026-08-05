//! Shared helpers for cuttlefish's daemon-facing tests (`daemon.rs`).
//!
//! Mirrors the equivalent helpers in `crates/cuttlefishd/tests/api.rs`: these
//! tests spawn a *real* `cuttlefishd` process and talk to it over a real
//! transport, nothing mocked.

use std::path::{Path, PathBuf};
use std::process::Stdio;

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

/// A spawned `cuttlefishd` child that is killed automatically when this value
/// is dropped — on a normal return, an early return, *or* a panic unwinding
/// through the test that owns it.
///
/// `cuttlefishd` never exits on its own; it just listens forever. Without
/// this guard, any fallible step between [`spawn_daemon`] and a test's own
/// cleanup (e.g. an `.unwrap()` on a warm-up call) would skip straight past
/// the manual `child.kill()` at the end and orphan the process.
pub struct DaemonGuard {
    child: std::process::Child,
}

impl DaemonGuard {
    /// Kill the daemon early, ignoring errors (e.g. it already exited).
    /// Calling this is optional — `Drop` kills it regardless — but doing so
    /// explicitly at a test's natural end can make the intent clearer.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // Best-effort: killing an already-exited child just errors, which we
        // don't care about here.
        let _ = self.child.kill();
    }
}

/// Spawn `cuttlefishd` against `spec_path`/`endpoint`, wait (briefly,
/// bounded) for it to actually accept connections before returning, and
/// return a [`DaemonGuard`] that kills it on drop no matter how the caller's
/// test function exits.
///
/// The child's stdout/stderr are piped (not inherited), mirroring
/// `crates/cuttlefishd/tests/api.rs`'s own `spawn_and_wait_ready` — otherwise
/// `cuttlefishd`'s own log lines (e.g. "listening on ...") print straight
/// into test output, bypassing the test harness's usual output capture.
///
/// Panics (after killing the child) if the daemon never becomes ready, so
/// callers only need to handle the happy path.
pub async fn spawn_daemon(spec_path: &Path, endpoint: &Path) -> DaemonGuard {
    let child = std::process::Command::new(cuttlefishd_binary())
        .arg(spec_path)
        .arg(endpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cuttlefishd failed to start");

    // Wrap in the guard immediately, before any further fallible step (e.g.
    // `client.build().unwrap()` below) — so a panic anywhere from here on
    // still kills the child via `Drop`, rather than only the explicit
    // `child.kill()` in the not-ready branch below covering it.
    let mut guard = DaemonGuard { child };

    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = builder.unix_socket(endpoint);
    #[cfg(windows)]
    let builder = builder.windows_named_pipe(endpoint);
    let client = builder.build().unwrap();

    let mut ready = false;
    for _ in 0..500 {
        if client.get("http://localhost/specs").send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    if !ready {
        guard.kill();
        panic!(
            "cuttlefishd did not become ready in time on {}",
            endpoint.display()
        );
    }
    guard
}
