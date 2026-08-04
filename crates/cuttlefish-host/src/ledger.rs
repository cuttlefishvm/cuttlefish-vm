//! Per-job durable checkpoint store. One SQLite file per job
//! (`$CUTTLEFISH_HOME/jobs/<job_id>/ledger.sqlite`), matching the catalog's
//! existing one-thing-per-file convention. See
//! docs/superpowers/specs/2026-08-03-dag-core-design.md's "Durability model"
//! for the full rationale — this module is purely storage; the resume
//! decision logic (skip on completed/skipped, run everything else) lives in
//! `crate::runner`.

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// A job's own terminal status, as recorded in the ledger — distinct from
/// per-node checkpoints, which alone can't tell "still running when the
/// process died" apart from "finished cleanly."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerJobStatus {
    /// The job has not yet called [`Ledger::finish`].
    Running,
    /// The job finished successfully.
    Completed,
    /// The job finished with an error.
    Failed,
    /// The job was cancelled before it finished.
    Cancelled,
}

impl LedgerJobStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Running,
        }
    }
}

/// A per-job checkpoint ledger, backed by a single SQLite file.
///
/// The connection is behind a `Mutex` purely to make `Ledger: Sync` —
/// `rusqlite::Connection` is `Send` but not `Sync` (its statement cache uses
/// unsynchronized interior mutability), and `run_job` holds a `&Ledger`
/// across `.await` points in a task spawned onto a multi-threaded runtime,
/// which requires the held reference to be `Send`, which in turn requires
/// `Ledger: Sync`. There is normally only ever one writer (the job that owns
/// this ledger), so contention is not a real concern; the lock exists to
/// satisfy the type system's threading rules, not to arbitrate real
/// concurrent access.
pub struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    /// Open (creating if absent) a job's ledger at `path`, ensuring both
    /// tables exist and `job_status` has its single row.
    ///
    /// `graph_fingerprint` is recorded only when `job_status` doesn't exist
    /// yet (a fresh job) — reopening an existing ledger leaves the
    /// originally-recorded fingerprint untouched, since comparing old vs.
    /// new fingerprint is a resume endpoint's job, not `open`'s.
    pub fn open(path: &Path, graph_fingerprint: &str) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        // A future reader (daemon startup scan, resume endpoint) may open
        // its own connection to this same file while a running job's
        // connection is mid-write. Without this, SQLite's default
        // busy_timeout of 0 means that second connection gets an immediate
        // SQLITE_BUSY instead of waiting briefly for the lock to clear.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                node_name    TEXT PRIMARY KEY,
                status       TEXT NOT NULL,
                output_json  TEXT,
                completed_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS job_status (status TEXT NOT NULL, graph_fingerprint TEXT NOT NULL);",
        )?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM job_status", [], |r| r.get(0))?;
        if count == 0 {
            conn.execute(
                "INSERT INTO job_status (status, graph_fingerprint) VALUES ('running', ?1)",
                [graph_fingerprint],
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The fingerprint recorded when this job was first submitted — compare
    /// against a freshly computed one before resuming.
    pub fn graph_fingerprint(&self) -> rusqlite::Result<String> {
        self.lock()
            .query_row("SELECT graph_fingerprint FROM job_status", [], |r| r.get(0))
    }

    /// The recorded output of `node_name`, if it completed successfully.
    /// `None` for a node that never ran, is still pending, or was skipped.
    pub fn get_completed(&self, node_name: &str) -> rusqlite::Result<Option<serde_json::Value>> {
        let result: Option<(String, Option<String>)> = match self.lock().query_row(
            "SELECT status, output_json FROM checkpoints WHERE node_name = ?1",
            [node_name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e),
        };
        match result {
            Some((status, Some(json))) if status == "completed" => Ok(Some(
                serde_json::from_str(&json).expect("ledger never stores invalid JSON"),
            )),
            _ => Ok(None),
        }
    }

    /// Whether `node_name` was recorded as skipped (e.g. excluded by a
    /// branch decision).
    pub fn is_skipped(&self, node_name: &str) -> rusqlite::Result<bool> {
        let status: Option<String> = match self.lock().query_row(
            "SELECT status FROM checkpoints WHERE node_name = ?1",
            [node_name],
            |r| r.get(0),
        ) {
            Ok(status) => Some(status),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e),
        };
        Ok(status.as_deref() == Some("skipped"))
    }

    /// Record `node_name` as completed with `output`, overwriting any prior
    /// checkpoint for that node.
    pub fn write_completed(
        &self,
        node_name: &str,
        output: &serde_json::Value,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints (node_name, status, output_json, completed_at)
             VALUES (?1, 'completed', ?2, ?3)",
            rusqlite::params![node_name, output.to_string(), now_marker()],
        )?;
        Ok(())
    }

    /// Record `node_name` as skipped, overwriting any prior checkpoint for
    /// that node.
    pub fn write_skipped(&self, node_name: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints (node_name, status, output_json, completed_at)
             VALUES (?1, 'skipped', NULL, ?2)",
            rusqlite::params![node_name, now_marker()],
        )?;
        Ok(())
    }

    /// The job's own terminal status. `Running` until [`Ledger::finish`] is
    /// called.
    pub fn job_status(&self) -> rusqlite::Result<LedgerJobStatus> {
        let s: String = self
            .lock()
            .query_row("SELECT status FROM job_status", [], |r| r.get(0))?;
        Ok(LedgerJobStatus::from_str(&s))
    }

    /// Record the job's terminal status (e.g. `"completed"`, `"failed"`,
    /// `"cancelled"`).
    pub fn finish(&self, status: &str) -> rusqlite::Result<()> {
        self.lock()
            .execute("UPDATE job_status SET status = ?1", [status])?;
        Ok(())
    }

    /// Lock the connection. The mutex is only ever held for the duration of
    /// one synchronous rusqlite call, never across an `.await` — so a
    /// poisoned lock can only mean a prior call panicked mid-query, an
    /// exceptional situation worth propagating loudly rather than papering
    /// over.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("ledger connection mutex poisoned")
    }
}

/// The root directory jobs live under. Checks `$CUTTLEFISH_JOBS_HOME` first
/// — set by `cuttlefish-run`'s project-scoping so a project's jobs/ledger
/// state lives under `<project>/.cuttlefish/jobs` without also redirecting
/// the (deliberately still-global) block catalog, which `$CUTTLEFISH_HOME`
/// alone continues to control. Falls back to `$CUTTLEFISH_HOME/jobs` (or
/// `~/.cuttlefish/jobs`) exactly as before when unset, so nothing about
/// existing single-global-home behavior changes for a caller that never
/// sets the new variable.
pub fn jobs_root() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CUTTLEFISH_JOBS_HOME") {
        return Some(std::path::PathBuf::from(dir));
    }
    crate::catalog::cuttlefish_home().map(|h| h.join("jobs"))
}

/// A completed_at marker. Plain wall-clock formatting, same as the
/// catalog's `now_rfc3339` (`crate::catalog`) — this is a diagnostic field,
/// not consulted by any resume logic, so precision/format choices here
/// don't affect correctness.
fn now_marker() -> String {
    crate::catalog::now_rfc3339()
}
