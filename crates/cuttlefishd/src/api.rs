//! The HTTP surface: submit a job, watch it, fetch its result, cancel it.
//!
//! See the crate docs for why this is HTTP over a unix socket rather than a
//! bespoke protocol.

use crate::state::{Job, JobStore};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use cuttlefish_core::spec::Spec;
use cuttlefish_host::{
    caps::Capabilities,
    infer::InferBackend,
    runner::{run_job, JobEvent, JobSpec},
};
use futures_util::{stream::Stream, StreamExt};
use serde::Deserialize;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState {
    /// Compiles and runs guest modules.
    pub engine: Arc<Engine>,
    /// Compiled-module cache, scoped to this process's single `engine` — see
    /// `cuttlefish_host::module_cache::ModuleCache`'s own doc comment for
    /// the invariant this depends on (never share one cache across two
    /// different `Engine`s).
    pub module_cache: Arc<cuttlefish_host::module_cache::ModuleCache>,
    /// Serves inference for the spec's own model.
    pub backend: Arc<dyn InferBackend>,
    /// Backends for models a node reaches only by explicit instruction — a
    /// `reroute` rung or a `Judge` naming its own model. Resolved at
    /// startup; see `cuttlefish_host::runner::Alternates`.
    pub alternates: Arc<cuttlefish_host::runner::Alternates>,
    /// Serves `embed`, when the spec declared an `embedding_model`.
    pub embedder: Option<Arc<dyn InferBackend>>,
    /// Job bookkeeping.
    pub jobs: JobStore,
    /// The one spec this daemon serves. A registry of many arrives later.
    pub spec: Arc<Spec>,
    /// The spec's checked nodes, in topological order, ready to execute.
    pub checked_nodes: Arc<Vec<cuttlefish_host::dag::CheckedNode>>,
    /// Which nodes are exclusive to which branch decision+label.
    pub exclusive_to:
        Arc<std::collections::HashMap<String, cuttlefish_host::dag::BranchExclusivity>>,
    /// Notified once, by the `/shutdown` handler, to trigger axum's graceful
    /// shutdown — see `serve::serve`. `Notify` (not a oneshot) because
    /// `AppState` is `Clone` and cheaply shared across every handler; a
    /// oneshot's single-consumption `Sender` doesn't fit a type that gets
    /// cloned per-request.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
}

/// A job submission.
#[derive(Deserialize)]
pub struct SubmitJob {
    /// Which spec to run; must match the loaded one.
    pub spec: String,
    /// Input handed to the block's `init`.
    pub input: serde_json::Value,
}

/// The read roots a job actually runs with: everything its spec declared,
/// plus its own fan-out results directory.
///
/// The daemon writes fan-out results itself, as host code, so nothing grants
/// *it* access. But a reduce block reading those results is sandboxed like
/// any other block, and the results path isn't knowable when the spec is
/// written — it depends on the job id. Granting it here, scoped to the one
/// directory this job's own results live in, is what lets a reduce node
/// `open` them without the spec author having to predict the path.
/// The warehouse plan for this spec, if it asked for one.
///
/// Built per job rather than stored on [`AppState`] because the lineage it
/// carries — model names above all — is what gets stamped on every row, and
/// reading it from the live spec here keeps one source for it rather than a
/// copy that can drift.
fn warehouse_plan(st: &AppState) -> Option<cuttlefish_host::runner::WarehousePlan> {
    st.spec
        .warehouse
        .as_ref()
        .map(|root| cuttlefish_host::runner::WarehousePlan {
            root: root.clone(),
            spec_name: st.spec.name.clone(),
            model: st.backend.model_name(),
            embedding_model: st.embedder.as_ref().map(|e| e.model_name()),
        })
}

fn fanout_aware_roots(st: &AppState, job_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut roots = st.spec.read_roots.clone();
    roots.push(job_dir.join("results"));
    roots
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    // Path parameters use axum 0.8 syntax — `{id}`, not the `:id` of 0.7.
    Router::new()
        .route("/specs", get(list_specs))
        .route("/jobs", get(list_jobs).post(submit))
        .route("/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/jobs/{id}/events", get(job_events))
        .route("/jobs/{id}/resume", post(resume_job))
        .route("/escalations", get(list_escalations))
        .route("/escalations/drain", post(drain_escalations))
        .route("/shutdown", post(shutdown))
        .with_state(state)
}

/// What this daemon can run.
///
/// This is the harness discovery endpoint: an agent reads `description` to
/// decide *whether* a job belongs here, which is why that field states trigger
/// conditions rather than summarising how the job works.
async fn list_specs(State(st): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!([{
        "name": st.spec.name,
        "description": st.spec.description,
        "data_policy": match st.spec.data_policy {
            cuttlefish_core::spec::DataPolicy::LocalOnly => "local_only",
            cuttlefish_core::spec::DataPolicy::Any => "any",
        },
    }]))
}

/// Everything every job on this machine gave up on.
///
/// Across *all* jobs, not just ones this daemon process ran. The whole reason
/// a node escalates is that the session which asked for the work is gone —
/// so scoping this to the current process, or to one job id the caller would
/// have to already know, would answer a question nobody has. The jobs root is
/// walked the same way `scan_for_interrupted_jobs` walks it at startup.
///
/// A single unreadable ledger is skipped with a warning rather than failing
/// the request: one job corrupted by a hard crash must not hide every other
/// job's escalations.
async fn list_escalations(Query(q): Query<EscalationQuery>) -> impl IntoResponse {
    match collect_escalations(q.all) {
        Ok(rows) => {
            Json(rows.into_iter().map(render_escalation).collect::<Vec<_>>()).into_response()
        }
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
    }
}

/// Export outstanding escalations and mark them drained, in one request.
///
/// Read-and-mark is deliberately a single operation. Splitting it into a
/// list call followed by a mark call would leave a window in which a second
/// caller drains the same rows into a second manifest — two planners, both
/// convinced they own the work.
///
/// Rows with no recorded input are returned as `unrecoverable` and are
/// *not* marked. They predate input recording, so there is nothing to hand
/// back; reporting them is what stops a drain from silently emitting fewer
/// lines than the queue listed.
async fn drain_escalations() -> impl IntoResponse {
    let found = match collect_escalations(false) {
        Ok(rows) => rows,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response()
        }
    };

    let (drainable, unrecoverable): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|(_, e)| e.input.is_some());

    // Mark only after building the response body, so a row is never
    // stamped for a payload the caller never received.
    let items: Vec<serde_json::Value> = drainable
        .iter()
        .map(|(job, e)| {
            serde_json::json!({
                "job": job,
                "node": e.node,
                "item": e.item,
                "input": e.input,
            })
        })
        .collect();

    let mut mark_errors: Vec<String> = Vec::new();
    for (job, e) in &drainable {
        match open_job_ledger(job) {
            Some(ledger) => {
                if let Err(err) = ledger.mark_drained(&e.node, e.item) {
                    mark_errors.push(format!("{job}/{}: {err}", e.node));
                }
            }
            None => mark_errors.push(format!("{job}: ledger vanished between read and mark")),
        }
    }

    Json(serde_json::json!({
        "items": items,
        "unrecoverable": unrecoverable
            .iter()
            .map(|(job, e)| serde_json::json!({
                "job": job, "node": e.node, "item": e.item, "reason": e.reason,
            }))
            .collect::<Vec<_>>(),
        // Surfaced rather than swallowed: the manifest is still valid, but
        // the caller needs to know these rows may come back on a later
        // drain.
        "mark_errors": mark_errors,
    }))
    .into_response()
}

/// Whether to include already-drained rows.
#[derive(Deserialize, Default)]
pub struct EscalationQuery {
    /// `?all=true` for the full history; absent means outstanding only.
    #[serde(default)]
    all: bool,
}

fn render_escalation((job, e): (String, cuttlefish_host::ledger::Escalation)) -> serde_json::Value {
    serde_json::json!({
        "job": job,
        "node": e.node,
        "item": e.item,
        "reason": e.reason,
        "at": e.at,
        "input": e.input,
        "drained_at": e.drained_at,
    })
}

fn open_job_ledger(job_id: &str) -> Option<cuttlefish_host::ledger::Ledger> {
    let path = cuttlefish_host::ledger::jobs_root()?
        .join(job_id)
        .join("ledger.sqlite");
    // The fingerprint is only consulted when creating a ledger, and this
    // one already exists.
    cuttlefish_host::ledger::Ledger::open(&path, "").ok()
}

/// Walk every job's ledger and gather escalations, newest scan each call.
///
/// Sorted by `(job, node, item)` before returning: `escalations()` orders
/// within one job, but `read_dir` order across jobs is unspecified, so
/// without this two drains of the same set would emit the same lines in
/// different orders.
///
/// A single unreadable ledger is skipped with a warning rather than failing
/// the request: one job corrupted by a hard crash must not hide every other
/// job's escalations.
fn collect_escalations(
    include_drained: bool,
) -> Result<Vec<(String, cuttlefish_host::ledger::Escalation)>, String> {
    let jobs_root = cuttlefish_host::ledger::jobs_root()
        .ok_or("could not determine home directory; set CUTTLEFISH_HOME")?;

    let mut all = Vec::new();
    if !jobs_root.exists() {
        return Ok(all);
    }
    let entries = std::fs::read_dir(&jobs_root)
        .map_err(|e| format!("reading {}: {e}", jobs_root.display()))?;

    for entry in entries.flatten() {
        let ledger_path = entry.path().join("ledger.sqlite");
        if !ledger_path.exists() {
            continue;
        }
        let job_id = entry.file_name().to_string_lossy().into_owned();
        // Read-only, and that is load-bearing rather than tidy. `Ledger::open`
        // runs CREATE TABLE / ALTER TABLE to bring a ledger up to date, and
        // DDL takes an exclusive lock — so listing escalations across every
        // job could lock the ledger of a job that is *currently running* and
        // fail it. Reading somebody else's ledger must never write to it.
        let ledger = match cuttlefish_host::ledger::Ledger::open_read_only(&ledger_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("warning: skipping unreadable ledger for job `{job_id}`: {e}");
                continue;
            }
        };
        let rows = if include_drained {
            ledger.all_escalations()
        } else {
            ledger.escalations()
        };
        match rows {
            Ok(rows) => all.extend(rows.into_iter().map(|e| (job_id.clone(), e))),
            Err(e) => eprintln!("warning: reading escalations for job `{job_id}`: {e}"),
        }
    }
    all.sort_by(|(ja, a), (jb, b)| {
        ja.cmp(jb)
            .then_with(|| a.node.cmp(&b.node))
            .then_with(|| a.item.cmp(&b.item))
    });
    Ok(all)
}

async fn submit(State(st): State<AppState>, Json(req): Json<SubmitJob>) -> impl IntoResponse {
    if req.spec != st.spec.name {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("unknown spec `{}`", req.spec) })),
        )
            .into_response();
    }

    let id = uuid::Uuid::new_v4().to_string();

    // A durable job directory, minted alongside the job_id: every job gets
    // its own on-disk ledger from the moment it's submitted, so a process
    // restart mid-job has something to resume from (later task) rather than
    // the job simply vanishing.
    let jobs_root = match cuttlefish_host::ledger::jobs_root() {
        Some(root) => root,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "could not determine home directory; set CUTTLEFISH_HOME"
                })),
            )
                .into_response();
        }
    };
    let job_dir = jobs_root.join(&id);
    if let Err(e) = std::fs::create_dir_all(&job_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("creating job directory {}: {e}", job_dir.display())
            })),
        )
            .into_response();
    }
    let fingerprint = cuttlefish_host::dag::graph_fingerprint(&st.checked_nodes);
    let ledger =
        match cuttlefish_host::ledger::Ledger::open(&job_dir.join("ledger.sqlite"), &fingerprint) {
            Ok(ledger) => ledger,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("opening ledger for job {id}: {e}")
                    })),
                )
                    .into_response();
            }
        };
    if let Err(e) = std::fs::write(job_dir.join("input.json"), req.input.to_string()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("writing input.json for job {id}: {e}")
            })),
        )
            .into_response();
    }

    let cancel = CancellationToken::new();
    st.jobs.insert(Job::new(id.clone(), cancel.clone())).await;

    let job_spec = JobSpec {
        nodes: (*st.checked_nodes).clone(),
        exclusive_to: (*st.exclusive_to).clone(),
        input: req.input,
        caps: Capabilities::new(fanout_aware_roots(&st, &job_dir))
            .with_fetch(st.spec.fetch_prefixes.clone()),
        alternates: (*st.alternates).clone(),
        embedder: st.embedder.clone(),
        warehouse: warehouse_plan(&st),
    };

    // Every event goes through `publish`, so it lands in the replay log as well
    // as the live channel — a client attaching after the job finished still sees
    // the whole stream.
    let (tx, mut rx) = mpsc::channel::<JobEvent>(256);
    let (forward_store, forward_id) = (st.jobs.clone(), id.clone());
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let payload = match event {
                JobEvent::Token(text) => serde_json::json!({ "type": "token", "text": text }),
                JobEvent::Progress(p) => serde_json::json!({ "type": "progress", "progress": p }),
            };
            forward_store
                .publish(&forward_id, payload.to_string())
                .await;
        }
    });

    let (engine, module_cache, backend, store, job_id) = (
        st.engine.clone(),
        st.module_cache.clone(),
        st.backend.clone(),
        st.jobs.clone(),
        id.clone(),
    );
    tokio::spawn(async move {
        let envelope = run_job(
            engine,
            backend,
            job_spec,
            tx,
            cancel,
            &ledger,
            &module_cache,
        )
        .await;
        store
            .publish(
                &job_id,
                serde_json::json!({ "type": "result", "envelope": envelope }).to_string(),
            )
            .await;
        store.finish(&job_id, envelope).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": id })),
    )
        .into_response()
}

/// Every job this daemon knows about, in the same shape as `get_job`.
///
/// This is what makes `Interrupted` jobs — surfaced only by the startup scan,
/// never pushed to anyone — actually discoverable: an agent lists jobs, sees
/// one sitting at `Interrupted`, and only then decides whether resuming it is
/// the right call. Resume is a signal, not an automatic action; this endpoint
/// is what delivers the signal.
async fn list_jobs(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.jobs.list().await)
}

/// Current status, and the envelope once the job has finished.
///
/// This is the recovery path: results live here as well as on the event stream,
/// so a dropped connection cannot lose one.
async fn get_job(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.jobs.get(&id).await {
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such job" })),
        )
            .into_response(),
        Some(job) => Json(serde_json::json!({
            "job_id": job.id,
            "status": job.status,
            "envelope": job.envelope,
        }))
        .into_response(),
    }
}

async fn cancel_job(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if st.jobs.cancel(&id).await {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Explicitly resume a job the startup scan found `Interrupted`.
///
/// Deliberately not automatic: per the design's "resume is a signal, not an
/// automatic action," an agent must see the job sitting at `Interrupted` (via
/// `GET /jobs`) and choose to call this — a crashed job never silently starts
/// running again on its own.
async fn resume_job(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let job = match st.jobs.get(&id).await {
        Some(job) if job.status == cuttlefish_abi::JobStatus::Interrupted => job,
        Some(_) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "only an Interrupted job can be resumed" })),
            )
                .into_response()
        }
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let jobs_root = match cuttlefish_host::ledger::jobs_root() {
        Some(root) => root,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "could not determine home directory; set CUTTLEFISH_HOME"
                })),
            )
                .into_response();
        }
    };
    let job_dir = jobs_root.join(&id);

    // The whole reason a graph_fingerprint is recorded: this daemon process
    // might be serving a different spec than the one that started this job
    // (an operator could have restarted it pointed at a different spec
    // file). Reject before touching the ledger's checkpoints at all —
    // resuming node-by-node against a mismatched graph would run some
    // nodes' blocks against a spec they were never checked against.
    let current_fingerprint = cuttlefish_host::dag::graph_fingerprint(&st.checked_nodes);
    let ledger = match cuttlefish_host::ledger::Ledger::open(
        &job_dir.join("ledger.sqlite"),
        &current_fingerprint,
    ) {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("opening ledger: {e}") })),
            )
                .into_response()
        }
    };
    let stored_fingerprint = match ledger.graph_fingerprint() {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("reading ledger: {e}") })),
            )
                .into_response()
        }
    };
    if stored_fingerprint != current_fingerprint {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "this job's graph fingerprint doesn't match the spec this daemon \
                          is currently serving — it was likely started against a different \
                          version of the spec. Refusing to resume rather than run its \
                          remaining nodes against a mismatched graph."
            })),
        )
            .into_response();
    }

    // Fingerprint verified — re-run using the same JobSpec-construction path
    // `submit` uses, reading input.json back from job_dir, and letting
    // run_job's ledger-aware loop skip everything already checkpointed via
    // the same ledger opened above.
    let input: serde_json::Value = match std::fs::read_to_string(job_dir.join("input.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "job directory has no readable input.json" })),
            )
                .into_response()
        }
    };
    let job_spec = JobSpec {
        nodes: (*st.checked_nodes).clone(),
        exclusive_to: (*st.exclusive_to).clone(),
        input,
        caps: Capabilities::new(fanout_aware_roots(&st, &job_dir))
            .with_fetch(st.spec.fetch_prefixes.clone()),
        alternates: (*st.alternates).clone(),
        embedder: st.embedder.clone(),
        warehouse: warehouse_plan(&st),
    };

    // Atomic guard against a concurrent second /resume call racing to this
    // same point — only one call may actually win and spawn run_job. This
    // deliberately sits last, after every fallible pre-flight check above
    // has already succeeded: flipping status any earlier would risk leaving
    // the job stuck in a new status with nothing running and no way to
    // resume it again, if a later check then failed.
    if st.jobs.try_start_resume(&id).await.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "this job is already being resumed" })),
        )
            .into_response();
    }

    let (engine, module_cache, backend, store, job_id) = (
        st.engine.clone(),
        st.module_cache.clone(),
        st.backend.clone(),
        st.jobs.clone(),
        id.clone(),
    );
    let cancel = job.cancel.clone();
    tokio::spawn(async move {
        // Events aren't replayed for a resume in v1 — a client watching
        // `/jobs/{id}/events` mid-crash has already lost that stream; the
        // result still lands durably via `finish`, same as any other job.
        let (tx, _rx) = mpsc::channel::<JobEvent>(256);
        let envelope = run_job(
            engine,
            backend,
            job_spec,
            tx,
            cancel,
            &ledger,
            &module_cache,
        )
        .await;
        store.finish(&job_id, envelope).await;
    });

    StatusCode::ACCEPTED.into_response()
}

/// Ask the daemon to stop accepting new connections and exit cleanly, once
/// any in-flight request finishes. Portable graceful-stop primitive for
/// `cuttlefish-run`'s "stop the old daemon, start a new one for a different
/// spec" flow — deliberately an HTTP route rather than a `SIGTERM` handler,
/// since Windows (which this daemon already supports via named pipes) has
/// no signal `cuttlefishd` could portably install a handler for.
///
/// A long-lived SSE subscriber on `/jobs/{id}/events` would otherwise block
/// axum's graceful shutdown indefinitely — it only closes a connection once
/// its current response finishes, and an SSE response doesn't finish until
/// the job completes *and* the client disconnects (`job_events`'s stream
/// stays open even after the job is done, so the store can still serve a
/// replay to a client that attaches later). So this also schedules a bounded
/// force-exit: if the process hasn't gone down gracefully within
/// `shutdown_grace()`, it exits anyway, guaranteeing `/shutdown` always
/// results in a bounded-time stop regardless of what any subscriber is doing.
async fn shutdown(State(st): State<AppState>) -> impl IntoResponse {
    st.shutdown.notify_one();
    tokio::spawn(async move {
        tokio::time::sleep(shutdown_grace()).await;
        std::process::exit(0);
    });
    StatusCode::ACCEPTED
}

/// The env var [`shutdown_grace`] checks before falling back to its default.
///
/// Read rather than a bare `const` so a test can shrink the wait to something
/// a test suite can afford to actually sit through — spawning the real
/// `cuttlefishd` binary is the only honest way to prove a *process* actually
/// exits (see `tests/api.rs`), and that test would otherwise cost the full
/// ten-second default per run. Production never sets this, so it always gets
/// the documented default.
const SHUTDOWN_GRACE_MS_ENV: &str = "CUTTLEFISH_SHUTDOWN_GRACE_MS";

/// How long `/shutdown` waits for axum's normal graceful path before forcing
/// the process down (see `shutdown`). Ten seconds comfortably exceeds any
/// single request this daemon serves — job submission, status checks, and
/// event replay all return in well under a second — while still being short
/// enough that the "stop the old daemon, start a new one" flow this
/// mechanism exists for never stalls noticeably from an operator's or
/// agent's point of view.
fn shutdown_grace() -> Duration {
    std::env::var(SHUTDOWN_GRACE_MS_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10))
}

/// Live event stream, replaying anything already emitted.
async fn job_events(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    let (backlog, rx) = st.jobs.subscribe(&id).await.ok_or(StatusCode::NOT_FOUND)?;

    let replay =
        futures_util::stream::iter(backlog.into_iter().map(|m| Ok(SseEvent::default().data(m))));
    let live = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| {
        // A lagging subscriber yields an error here; drop it rather than ending
        // the stream, since the client can recover the full history from the
        // backlog on reconnect.
        futures_util::future::ready(msg.ok().map(|m| Ok(SseEvent::default().data(m))))
    });

    Ok(Sse::new(replay.chain(live)))
}
