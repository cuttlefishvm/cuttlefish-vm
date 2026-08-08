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
    job_dir: std::path::PathBuf,
}

/// Names the job this ledger belongs to without trying to render the SQLite
/// connection, which is not `Debug`. Exists so callers can use
/// `Result`-combinators like `expect_err` on [`Ledger::open`].
impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("job_dir", &self.job_dir)
            .finish_non_exhaustive()
    }
}

/// One thing a job gave up on after its recovery ladder was exhausted.
///
/// Carries enough for a session that wasn't there when it happened to act:
/// which node, which item if any, and *why*. A list of job ids would only
/// be a second hunt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Escalation {
    /// The node that gave up.
    pub node: String,
    /// Which fan-out item, or `None` for a whole node.
    pub item: Option<usize>,
    /// The failure that exhausted the ladder.
    pub reason: String,
    /// When it was recorded.
    pub at: String,
}

/// Why a ledger could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The underlying SQLite call failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The file predates per-item checkpoints, so its `checkpoints` table has
    /// no `item_index` column and its rows cannot be interpreted against the
    /// current schema.
    #[error(
        "this job's ledger predates per-item fan-out checkpoints and cannot be resumed \
         — re-submit the job to start a fresh one"
    )]
    StaleSchema,
}

impl Ledger {
    /// Open (creating if absent) a job's ledger at `path`, ensuring both
    /// tables exist and `job_status` has its single row.
    ///
    /// `graph_fingerprint` is recorded only when `job_status` doesn't exist
    /// yet (a fresh job) — reopening an existing ledger leaves the
    /// originally-recorded fingerprint untouched, since comparing old vs.
    /// new fingerprint is a resume endpoint's job, not `open`'s.
    pub fn open(path: &Path, graph_fingerprint: &str) -> Result<Self, LedgerError> {
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

        // A table that already exists keeps its original shape under CREATE
        // TABLE IF NOT EXISTS, so a ledger written before per-item
        // checkpoints would survive to here and then fail at the first
        // INSERT with "no such column: item_index" — an error that says
        // nothing about what actually happened or what to do. Detect it here
        // instead. An empty result means the table doesn't exist yet, which
        // is just a fresh ledger.
        let existing_columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(checkpoints)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if !existing_columns.is_empty() && !existing_columns.iter().any(|c| c == "item_index") {
            return Err(LedgerError::StaleSchema);
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                node_name    TEXT NOT NULL,
                item_index   INTEGER NOT NULL DEFAULT -1,
                status       TEXT NOT NULL,
                output_json  TEXT,
                error_text   TEXT,
                completed_at TEXT NOT NULL,
                PRIMARY KEY (node_name, item_index)
             );
             CREATE TABLE IF NOT EXISTS job_status (status TEXT NOT NULL, graph_fingerprint TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS fanout_manifests (
                node_name  TEXT PRIMARY KEY,
                digest     TEXT NOT NULL,
                item_count INTEGER NOT NULL
             );",
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
            // Fan-out results are materialized beside the ledger. Deriving
            // the directory from the path we were already given avoids
            // threading a second, independently-computed notion of "this
            // job's directory" through `run_job`, which could drift.
            job_dir: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
        })
    }

    /// The directory this job's state lives in — the ledger file's own
    /// parent. Fan-out results are materialized under here.
    pub fn job_dir(&self) -> &Path {
        &self.job_dir
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
            "SELECT status, output_json FROM checkpoints WHERE node_name = ?1 AND item_index = -1",
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
            "SELECT status FROM checkpoints WHERE node_name = ?1 AND item_index = -1",
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
            "INSERT OR REPLACE INTO checkpoints
               (node_name, item_index, status, output_json, error_text, completed_at)
             VALUES (?1, -1, 'completed', ?2, NULL, ?3)",
            rusqlite::params![node_name, output.to_string(), now_marker()],
        )?;
        Ok(())
    }

    /// Record `node_name` as skipped, overwriting any prior checkpoint for
    /// that node.
    pub fn write_skipped(&self, node_name: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints
               (node_name, item_index, status, output_json, error_text, completed_at)
             VALUES (?1, -1, 'skipped', NULL, NULL, ?2)",
            rusqlite::params![node_name, now_marker()],
        )?;
        Ok(())
    }

    /// Record one fan-out item as completed with `output`.
    pub fn write_item_completed(
        &self,
        node_name: &str,
        item_index: usize,
        output: &serde_json::Value,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints
               (node_name, item_index, status, output_json, error_text, completed_at)
             VALUES (?1, ?2, 'completed', ?3, NULL, ?4)",
            rusqlite::params![
                node_name,
                item_index as i64,
                output.to_string(),
                now_marker()
            ],
        )?;
        Ok(())
    }

    /// Record one fan-out item as having *concluded* in failure.
    ///
    /// Concluded is the operative word. An item still in flight when the
    /// process died must leave no row at all, so that resume re-runs it —
    /// whereas an item whose block genuinely returned `Fail` is recorded
    /// here and never retried. That distinction is the entire basis of
    /// fan-out resume semantics: it separates "this chunk is bad" from "we
    /// were interrupted", without needing to ask which happened.
    pub fn write_item_failed(
        &self,
        node_name: &str,
        item_index: usize,
        error: &str,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints
               (node_name, item_index, status, output_json, error_text, completed_at)
             VALUES (?1, ?2, 'failed', NULL, ?3, ?4)",
            rusqlite::params![node_name, item_index as i64, error, now_marker()],
        )?;
        Ok(())
    }

    /// One item's recorded output, if it completed successfully.
    pub fn get_item_completed(
        &self,
        node_name: &str,
        item_index: usize,
    ) -> rusqlite::Result<Option<serde_json::Value>> {
        let row: Option<(String, Option<String>)> = match self.lock().query_row(
            "SELECT status, output_json FROM checkpoints
             WHERE node_name = ?1 AND item_index = ?2",
            rusqlite::params![node_name, item_index as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e),
        };
        Ok(match row {
            Some((status, Some(json))) if status == "completed" => {
                Some(serde_json::from_str(&json).expect("ledger never stores invalid JSON"))
            }
            _ => None,
        })
    }

    /// Whether this item already concluded, either way — the resume check.
    /// A concluded item is skipped; anything else is (re-)run.
    pub fn item_concluded(&self, node_name: &str, item_index: usize) -> rusqlite::Result<bool> {
        let count: i64 = self.lock().query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE node_name = ?1 AND item_index = ?2",
            rusqlite::params![node_name, item_index as i64],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Every concluded item for `node_name`, in index order, as
    /// `(index, output, error)` — exactly one of `output`/`error` is `Some`.
    #[allow(clippy::type_complexity)]
    pub fn concluded_items(
        &self,
        node_name: &str,
    ) -> rusqlite::Result<Vec<(usize, Option<serde_json::Value>, Option<String>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT item_index, status, output_json, error_text FROM checkpoints
             WHERE node_name = ?1 AND item_index >= 0 ORDER BY item_index",
        )?;
        let rows = stmt.query_map([node_name], |r| {
            let index: i64 = r.get(0)?;
            let status: String = r.get(1)?;
            let output: Option<String> = r.get(2)?;
            let error: Option<String> = r.get(3)?;
            Ok((
                index as usize,
                if status == "completed" {
                    output.map(|j| {
                        serde_json::from_str(&j).expect("ledger never stores invalid JSON")
                    })
                } else {
                    None
                },
                // Anything not `completed` is a failure of some kind, and
                // carries its error. Matching on `failed` alone would drop
                // `escalated` items out of `failures.jsonl` entirely — they
                // would count as concluded, count toward `failed`, and then
                // silently vanish from the projection.
                if status == "completed" { None } else { error },
            ))
        })?;
        rows.collect()
    }

    /// Record that recovery was exhausted for `node_name`, giving up.
    ///
    /// Stored as an ordinary concluded failure with a distinct status rather
    /// than in a table of its own — the composite key already carries node
    /// and item, and an escalation *is* a kind of concluded failure. Pass
    /// `None` for a whole node, `Some(i)` for one fan-out item.
    pub fn write_escalated(
        &self,
        node_name: &str,
        item_index: Option<usize>,
        reason: &str,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO checkpoints
               (node_name, item_index, status, output_json, error_text, completed_at)
             VALUES (?1, ?2, 'escalated', NULL, ?3, ?4)",
            rusqlite::params![
                node_name,
                item_index.map(|i| i as i64).unwrap_or(-1),
                reason,
                now_marker()
            ],
        )?;
        Ok(())
    }

    /// Everything this job gave up on, for `cuttlefish escalations`.
    pub fn escalations(&self) -> rusqlite::Result<Vec<Escalation>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT node_name, item_index, error_text, completed_at FROM checkpoints
             WHERE status = 'escalated' ORDER BY node_name, item_index",
        )?;
        let rows = stmt.query_map([], |r| {
            let index: i64 = r.get(1)?;
            Ok(Escalation {
                node: r.get(0)?,
                // -1 is the whole-node sentinel, not a real item.
                item: (index >= 0).then_some(index as usize),
                reason: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Record what `node_name` fanned out over, or verify it is unchanged.
    ///
    /// `Ok(Err(previous_digest))` means this node previously ran against a
    /// *different* manifest. Item indices are only meaningful relative to one
    /// specific manifest, so resuming would quietly pair recorded results
    /// with entirely different inputs — no graph-level fingerprint can catch
    /// an edit to the manifest file itself, which is why this exists.
    ///
    /// The outer `Result` is storage failure; the inner one is the verdict.
    pub fn check_or_record_manifest(
        &self,
        node_name: &str,
        digest: &str,
        item_count: usize,
    ) -> rusqlite::Result<Result<(), String>> {
        let conn = self.lock();
        let existing: Option<String> = match conn.query_row(
            "SELECT digest FROM fanout_manifests WHERE node_name = ?1",
            [node_name],
            |r| r.get(0),
        ) {
            Ok(d) => Some(d),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e),
        };
        match existing {
            Some(previous) if previous != digest => Ok(Err(previous)),
            Some(_) => Ok(Ok(())),
            None => {
                conn.execute(
                    "INSERT INTO fanout_manifests (node_name, digest, item_count)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![node_name, digest, item_count as i64],
                )?;
                Ok(Ok(()))
            }
        }
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
