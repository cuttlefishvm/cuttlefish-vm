//! Per-job durable checkpoint store. One SQLite file per job
//! (`$CUTTLEFISH_HOME/jobs/<job_id>/ledger.sqlite`), matching the catalog's
//! existing one-thing-per-file convention. See
//! docs/superpowers/specs/2026-08-03-dag-core-design.md's "Durability model"
//! for the full rationale — this module is purely storage; the resume
//! decision logic (skip on completed/skipped, run everything else) lives in
//! `crate::runner`.

use rusqlite::Connection;
use std::path::Path;

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
pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Open (creating if absent) a job's ledger at `path`, ensuring both
    /// tables exist and `job_status` has its single row.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                node_name    TEXT PRIMARY KEY,
                status       TEXT NOT NULL,
                output_json  TEXT,
                completed_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS job_status (status TEXT NOT NULL);",
        )?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM job_status", [], |r| r.get(0))?;
        if count == 0 {
            conn.execute("INSERT INTO job_status (status) VALUES ('running')", [])?;
        }
        Ok(Self { conn })
    }

    /// The recorded output of `node_name`, if it completed successfully.
    /// `None` for a node that never ran, is still pending, or was skipped.
    pub fn get_completed(&self, node_name: &str) -> rusqlite::Result<Option<serde_json::Value>> {
        let result: Option<(String, Option<String>)> = match self.conn.query_row(
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
        let status: Option<String> = match self.conn.query_row(
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
        self.conn.execute(
            "INSERT OR REPLACE INTO checkpoints (node_name, status, output_json, completed_at)
             VALUES (?1, 'completed', ?2, ?3)",
            rusqlite::params![node_name, output.to_string(), now_marker()],
        )?;
        Ok(())
    }

    /// Record `node_name` as skipped, overwriting any prior checkpoint for
    /// that node.
    pub fn write_skipped(&self, node_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
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
            .conn
            .query_row("SELECT status FROM job_status", [], |r| r.get(0))?;
        Ok(LedgerJobStatus::from_str(&s))
    }

    /// Record the job's terminal status (e.g. `"completed"`, `"failed"`,
    /// `"cancelled"`).
    pub fn finish(&self, status: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("UPDATE job_status SET status = ?1", [status])?;
        Ok(())
    }
}

/// A completed_at marker. Plain wall-clock formatting, same as the
/// catalog's `now_rfc3339` (`crate::catalog`) — this is a diagnostic field,
/// not consulted by any resume logic, so precision/format choices here
/// don't affect correctness.
fn now_marker() -> String {
    crate::catalog::now_rfc3339()
}
