//! Job bookkeeping: status, results, and the event log.
//!
//! # Why events are logged and not merely broadcast
//!
//! `tokio::sync::broadcast` has no replay for late subscribers. `send` returns
//! early when the receiver count is zero and does not even write to the ring
//! buffer, and `subscribe` starts a receiver at the current tail.
//!
//! That is fatal for the obvious client shape: submit a job, then connect to its
//! event stream. Those are two round trips, and a fast job finishes during the
//! gap — so a broadcast-only design delivers *nothing*, reliably, and looks like
//! a streaming bug rather than a design one.
//!
//! So every event goes into a per-job log as well as the live channel, and
//! subscribing hands back the backlog together with a receiver. Finished
//! envelopes are retained for the same reason: a dropped connection must not be
//! able to lose a result that the daemon already computed.

use cuttlefish_abi::{Envelope, JobStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

/// How many live events a subscriber may fall behind before it is dropped.
///
/// Falling behind is survivable rather than fatal: the log holds everything, so
/// a lagging subscriber that reconnects catches up from the backlog.
const BROADCAST_CAPACITY: usize = 256;

/// One job's state.
#[derive(Clone)]
pub struct Job {
    /// Unique identifier, minted at submission.
    pub id: String,
    /// Lifecycle state.
    pub status: JobStatus,
    /// Present once the job reaches a terminal state.
    pub envelope: Option<Envelope>,
    /// Cancels the running job; see the runner.
    pub cancel: CancellationToken,
    /// Live events, for subscribers already attached.
    pub events: broadcast::Sender<String>,
    /// Everything ever published for this job, so a late subscriber catches up.
    ///
    /// Unbounded, which is fine at this slice's scale — a job emits tens of
    /// events. A cap belongs here once jobs run long enough for it to matter.
    pub log: Vec<String>,
}

impl Job {
    /// Create a queued job with a fresh event channel.
    pub fn new(id: String, cancel: CancellationToken) -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            id,
            status: JobStatus::Running,
            envelope: None,
            cancel,
            events,
            log: Vec::new(),
        }
    }

    /// Reconstruct a job discovered mid-flight in its ledger at daemon
    /// startup — not actually running (there is no task backing it), just
    /// recorded as having been running when the process last died.
    ///
    /// The `CancellationToken` is fresh and never cancelled: nothing is
    /// executing for this job in this process, so there is nothing for it to
    /// cancel. `envelope` stays `None` — there is nothing to show yet; a
    /// resume endpoint is what eventually gives this job a real envelope.
    pub fn interrupted(id: String) -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            id,
            status: JobStatus::Interrupted,
            envelope: None,
            cancel: CancellationToken::new(),
            events,
            log: Vec::new(),
        }
    }
}

/// Every job this daemon knows about.
#[derive(Clone, Default)]
pub struct JobStore {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
}

impl JobStore {
    /// Record a new job.
    pub async fn insert(&self, job: Job) {
        self.jobs.lock().await.insert(job.id.clone(), job);
    }

    /// Fetch a snapshot of a job, if it exists.
    pub async fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().await.get(id).cloned()
    }

    /// Append to the replay log and fan out to live subscribers.
    ///
    /// Both happen under one lock so an event can never be simultaneously missed
    /// by a subscriber and absent from the log. A failing `send` just means
    /// nobody is attached yet, which the log covers.
    pub async fn publish(&self, id: &str, payload: String) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            job.log.push(payload.clone());
            let _ = job.events.send(payload);
        }
    }

    /// Snapshot the backlog and attach a live receiver.
    ///
    /// Both under the *same* lock guard. Splitting them would reopen the gap
    /// this exists to close: an event published between the snapshot and the
    /// subscribe would appear in neither.
    pub async fn subscribe(&self, id: &str) -> Option<(Vec<String>, broadcast::Receiver<String>)> {
        let jobs = self.jobs.lock().await;
        let job = jobs.get(id)?;
        Some((job.log.clone(), job.events.subscribe()))
    }

    /// Record a job's terminal envelope.
    pub async fn finish(&self, id: &str, envelope: Envelope) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            job.status = envelope.status;
            job.envelope = Some(envelope);
        }
    }

    /// Record a job discovered mid-flight in its ledger at daemon startup;
    /// see [`Job::interrupted`].
    pub async fn mark_interrupted(&self, id: String) {
        self.insert(Job::interrupted(id)).await;
    }

    /// Request cancellation. Returns whether the job existed.
    ///
    /// Best-effort by nature: a job that has already finished is reported as
    /// cancelled-successfully because the caller's intent — "do not keep running
    /// this" — is already satisfied.
    pub async fn cancel(&self, id: &str) -> bool {
        match self.jobs.lock().await.get(id) {
            Some(job) => {
                job.cancel.cancel();
                true
            }
            None => false,
        }
    }
}
