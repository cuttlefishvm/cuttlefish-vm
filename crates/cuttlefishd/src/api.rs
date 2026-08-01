//! The HTTP surface: submit a job, watch it, fetch its result, cancel it.
//!
//! See the crate docs for why this is HTTP over a unix socket rather than a
//! bespoke protocol.

use crate::state::{Job, JobStore};
use axum::{
    extract::{Path, State},
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
use std::{convert::Infallible, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// Everything the handlers share.
#[derive(Clone)]
pub struct AppState {
    /// Compiles and runs guest modules.
    pub engine: Arc<Engine>,
    /// Serves inference.
    pub backend: Arc<dyn InferBackend>,
    /// Job bookkeeping.
    pub jobs: JobStore,
    /// The one spec this daemon serves. A registry of many arrives later.
    pub spec: Arc<Spec>,
    /// The compiled block implementing that spec.
    pub module_bytes: Arc<Vec<u8>>,
}

/// A job submission.
#[derive(Deserialize)]
pub struct SubmitJob {
    /// Which spec to run; must match the loaded one.
    pub spec: String,
    /// Input handed to the block's `init`.
    pub input: serde_json::Value,
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    // Path parameters use axum 0.8 syntax — `{id}`, not the `:id` of 0.7.
    Router::new()
        .route("/specs", get(list_specs))
        .route("/jobs", post(submit))
        .route("/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/jobs/{id}/events", get(job_events))
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

async fn submit(State(st): State<AppState>, Json(req): Json<SubmitJob>) -> impl IntoResponse {
    if req.spec != st.spec.name {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("unknown spec `{}`", req.spec) })),
        )
            .into_response();
    }

    let id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    st.jobs.insert(Job::new(id.clone(), cancel.clone())).await;

    let job_spec = JobSpec {
        module_bytes: (*st.module_bytes).clone(),
        input: req.input,
        caps: Capabilities::new(st.spec.read_roots.clone()),
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

    let (engine, backend, store, job_id) = (
        st.engine.clone(),
        st.backend.clone(),
        st.jobs.clone(),
        id.clone(),
    );
    tokio::spawn(async move {
        let envelope = run_job(engine, backend, job_spec, tx, cancel).await;
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
