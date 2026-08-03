//! Cross-process catalog contention, driven through the real `cuttlefish`
//! binary.
//!
//! The catalog's index lock is an OS-level advisory lock on
//! `index.json.lock`. `cuttlefish-host`'s own tests race *threads* inside one
//! process, which exercises the lock but not the case the lock actually
//! exists for: two people (or an agent and a script) running `cuttlefish
//! catalog add` at the same time in separate processes. These tests spawn
//! genuine concurrent processes so a regression that only shows up across
//! process boundaries — a lock taken too late, a lock scoped to the wrong
//! file, a read-modify-write that escapes it — cannot pass unnoticed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The whole of a valid, empty wasm module: magic + version. `catalog add`
/// sniffs kind from these bytes and accepts it (with the no-signature
/// warning), which is all these tests need — compiling a real block would
/// cost minutes and prove nothing extra about locking.
const EMPTY_WASM: &[u8] = b"\x00asm\x01\x00\x00\x00";

const CUTTLEFISH: &str = env!("CARGO_BIN_EXE_cuttlefish");

/// Spawn `catalog add <name_version> <wasm>` against `home` without waiting
/// on it, so every child in a batch is in flight at once.
fn spawn_add(home: &Path, wasm: &Path, name_version: &str) -> std::process::Child {
    Command::new(CUTTLEFISH)
        .args(["catalog", "add", name_version])
        .arg(wasm)
        .env("CUTTLEFISH_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the cuttlefish binary under test must be spawnable")
}

fn write_wasm(dir: &Path) -> PathBuf {
    let path = dir.join("empty.wasm");
    std::fs::write(&path, EMPTY_WASM).unwrap();
    path
}

fn list_lines(home: &Path) -> Vec<String> {
    let out = Command::new(CUTTLEFISH)
        .args(["catalog", "list"])
        .env("CUTTLEFISH_HOME", home)
        .output()
        .expect("catalog list must run");
    assert!(
        out.status.success(),
        "catalog list failed after contention: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Every distinct name added concurrently by a separate process must survive.
/// A read-modify-write that escaped the lock would drop entries here: each
/// process reads the index, inserts one key, and writes the whole file back,
/// so a lost update silently erases another process's entry.
#[test]
fn concurrent_add_processes_with_distinct_names_all_land() {
    const PROCESSES: usize = 8;

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let wasm = write_wasm(src.path());

    let children: Vec<_> = (0..PROCESSES)
        .map(|i| spawn_add(home.path(), &wasm, &format!("stress-{i}@1")))
        .collect();

    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "concurrent add of stress-{i}@1 failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let lines = list_lines(home.path());
    assert_eq!(
        lines.len(),
        PROCESSES,
        "every concurrent add must survive; a short list means a lost update \
         across processes: {lines:#?}"
    );
    for i in 0..PROCESSES {
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("stress-{i}@1"))),
            "stress-{i}@1 vanished from the index: {lines:#?}"
        );
    }
}

/// The immutability promise ("versions are immutable once published") has to
/// hold when the racers are separate processes, not just separate threads:
/// exactly one process may win a given `name@version`, and every loser must
/// fail loudly rather than silently overwrite.
#[test]
fn concurrent_add_processes_racing_one_name_produce_exactly_one_winner() {
    const PROCESSES: usize = 8;

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let wasm = write_wasm(src.path());

    let children: Vec<_> = (0..PROCESSES)
        .map(|_| spawn_add(home.path(), &wasm, "race@1"))
        .collect();

    let mut winners = 0;
    let mut losers = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if out.status.success() {
            winners += 1;
        } else {
            losers += 1;
            assert!(
                stderr.contains("already catalogued"),
                "a loser of the race must fail with the immutability error, \
                 not an io or corruption error: {stderr}"
            );
        }
    }

    assert_eq!(
        winners, 1,
        "exactly one process may claim race@1 ({winners} won, {losers} lost)"
    );

    let lines = list_lines(home.path());
    assert_eq!(
        lines.len(),
        1,
        "race@1 must appear exactly once: {lines:#?}"
    );
}

/// `list` takes no lock at all, so it must still succeed — and still see
/// every entry that was never removed — while a pack of writer processes
/// hammers the same catalog.
///
/// Scope, deliberately stated because it is easy to over-read: this catches a
/// reader that errors out or loses entries under contention (verified: it
/// fails reliably if the index lock is removed). It is *not* a sensitive
/// guard on the temp-file-then-rename publish step. Each read here is a fresh
/// process, and process startup (milliseconds) dwarfs the window in which a
/// torn index would be visible (microseconds), so a regression to
/// write-in-place slips past this test — confirmed by mutation, it stays
/// green. The test with real sensitivity to that is
/// `lock_free_readers_never_observe_a_partial_index_while_writers_hammer` in
/// `cuttlefish-host`'s `catalog` unit tests, which reads in-process in a tight
/// loop and does go red. Keep both: this one covers the real CLI wiring, that
/// one covers the atomicity.
#[test]
fn a_lock_free_list_keeps_succeeding_while_writer_processes_are_active() {
    const WRITERS: usize = 6;
    const READERS: usize = 4;

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let wasm = write_wasm(src.path());

    // Seed so index.json exists before the readers start; a missing index is
    // legitimately an empty catalog and would mask a torn read.
    let seed = spawn_add(home.path(), &wasm, "seed@1")
        .wait_with_output()
        .unwrap();
    assert!(seed.status.success());

    let writers: Vec<_> = (0..WRITERS)
        .map(|i| spawn_add(home.path(), &wasm, &format!("w-{i}@1")))
        .collect();

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let home = home.path().to_path_buf();
            std::thread::spawn(move || {
                // Each iteration is a fresh process doing a lock-free read.
                for _ in 0..12 {
                    let out = Command::new(CUTTLEFISH)
                        .args(["catalog", "list"])
                        .env("CUTTLEFISH_HOME", &home)
                        .output()
                        .expect("catalog list must run");
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    assert!(
                        out.status.success(),
                        "a lock-free reader saw a bad index mid-write: {stderr}"
                    );
                    assert!(
                        !stderr.contains("corrupt"),
                        "a lock-free reader must never report corruption: {stderr}"
                    );
                    assert!(
                        String::from_utf8_lossy(&out.stdout).contains("seed@1"),
                        "an entry that is never removed vanished from a concurrent read"
                    );
                }
            })
        })
        .collect();

    for (i, child) in writers.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "writer w-{i}@1 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for r in readers {
        r.join()
            .expect("no reader may observe a corrupt or partial index");
    }

    let lines = list_lines(home.path());
    assert_eq!(lines.len(), WRITERS + 1, "{lines:#?}");
}
