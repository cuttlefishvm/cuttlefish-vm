//! The reactor loop: the host drives the guest, one command at a time.
//!
//! This module is the shape of the whole system. The guest never calls the host
//! and waits — it returns a [`Command`], the host carries it out, and the host
//! steps the guest again with the resulting [`Event`]. Two things follow from
//! that inversion, and both are why it is worth the awkwardness:
//!
//! - **Cancellation needs no guest cooperation.** The host simply stops
//!   stepping. A guest cannot ignore, delay, or trap its way out of it.
//! - **Every iteration is observable.** Progress, token counts, and capability
//!   decisions all pass through the host, even for a block whose internal loop
//!   the DAG cannot see.
//!
//! The alternative — host functions the guest imports and blocks on — is not
//! merely less tidy, it does not work: a single-threaded core-wasm guest offers
//! no execution context for the host to call back into, and the wasmtime `Store`
//! is `!Sync` while inference must run on a separate thread.

use crate::caps::Capabilities;
use crate::handles::Handles;
use crate::infer::{InferBackend, InferRequest, InferResult};
use cuttlefish_abi::{error_codes, Command, Envelope, Event, JobError, JobStatus, Usage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// Width, in pixels, that document pages render to.
///
/// Vision models work from a fixed-size input anyway, and a larger raster costs
/// encode time and tokens without adding detail the model can use.
const RENDER_WIDTH: u16 = 1024;

/// Something worth telling a watcher about while a job runs.
#[derive(Debug, Clone)]
pub enum JobEvent {
    /// One generated token.
    Token(String),
    /// Guest-supplied progress.
    Progress(serde_json::Value),
}

/// Everything needed to run one job.
pub struct JobSpec {
    /// The checked graph, in topological order — safe to execute
    /// front-to-back, threading `outputs` forward.
    pub nodes: Vec<crate::dag::CheckedNode>,
    /// Which nodes are exclusive to which branch decision+label — see
    /// `crate::dag::BranchExclusivity`.
    pub exclusive_to: std::collections::HashMap<String, crate::dag::BranchExclusivity>,
    /// The job's input, handed to every entry node (a node with no `input`
    /// expression — `node.input.is_none()`).
    pub input: serde_json::Value,
    /// What this job is permitted to reach.
    pub caps: Capabilities,
}

/// Pointer width of a guest module, read from the module rather than assumed.
///
/// Only [`Abi::W32`] is supported today; 64-bit guests are rejected with a clear
/// message. The enum exists anyway so that adding wasm64 later is a new arm plus
/// a second set of [`TypedFunc`] signatures, rather than a hunt through this
/// file for every place a pointer was assumed to be four bytes wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    /// 32-bit linear memory.
    W32,
    /// 64-bit linear memory (memory64).
    W64,
}

impl Abi {
    /// Size of one pointer-sized field, and so half a descriptor.
    fn ptr_size(self) -> usize {
        match self {
            Abi::W32 => 4,
            Abi::W64 => 8,
        }
    }
}

struct Guest {
    store: Store<()>,
    memory: Memory,
    abi: Abi,
    alloc: TypedFunc<u32, u32>,
    init: TypedFunc<(u32, u32), u32>,
    step: TypedFunc<(u32, u32), u32>,
    on_token: Option<TypedFunc<(u32, u32), i32>>,
}

impl Guest {
    fn new(engine: &Engine, module_bytes: &[u8]) -> anyhow::Result<Self> {
        let module = Module::new(engine, module_bytes)?;

        // An empty linker, deliberately. Guest blocks are built for
        // `wasm32-unknown-unknown` and import nothing at all — a wasip1 guest
        // would drag in `fd_write` and `proc_exit` through its panic path alone
        // and fail to instantiate here.
        let linker: Linker<()> = Linker::new(engine);
        let mut store = Store::new(engine, ());
        let instance: Instance = linker.instantiate(&mut store, &module)?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow::anyhow!("guest exports no memory"))?;

        // Width comes from the module itself. A 64-bit guest exports `cf_init`
        // as `(i64, i64) -> i64`, so the typed lookups below would otherwise
        // fail with a signature mismatch that says nothing about the real cause.
        let abi = if memory.ty(&store).is_64() {
            Abi::W64
        } else {
            Abi::W32
        };
        if abi == Abi::W64 {
            anyhow::bail!("guest uses 64-bit memory; only 32-bit guests are supported");
        }

        Ok(Self {
            alloc: instance.get_typed_func(&mut store, "cf_alloc")?,
            init: instance.get_typed_func(&mut store, "cf_init")?,
            step: instance.get_typed_func(&mut store, "cf_step")?,
            // Optional: a block indifferent to streaming need not export it.
            on_token: instance.get_typed_func(&mut store, "cf_on_token").ok(),
            memory,
            abi,
            store,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<(u32, u32)> {
        let len = bytes.len() as u32;
        let ptr = self.alloc.call(&mut self.store, len)?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok((ptr, len))
    }

    /// Read the descriptor the guest returned, then the payload it points at.
    ///
    /// Two reads rather than unpacking one integer — the cost of keeping these
    /// signatures identical across pointer widths.
    fn read_desc(&mut self, desc_ptr: u32) -> anyhow::Result<Vec<u8>> {
        let w = self.abi.ptr_size();
        let mut desc = vec![0u8; 2 * w];
        self.memory
            .read(&mut self.store, desc_ptr as usize, &mut desc)?;

        let field = |bytes: &[u8]| -> u64 {
            match w {
                4 => u32::from_le_bytes(bytes.try_into().expect("4 bytes")) as u64,
                _ => u64::from_le_bytes(bytes.try_into().expect("8 bytes")),
            }
        };
        let ptr = field(&desc[..w]) as usize;
        let len = field(&desc[w..]) as usize;

        let mut buf = vec![0u8; len];
        self.memory.read(&mut self.store, ptr, &mut buf)?;
        Ok(buf)
    }

    fn call_init(&mut self, input: &serde_json::Value) -> anyhow::Result<Command> {
        let bytes = serde_json::to_vec(input)?;
        let (ptr, len) = self.write(&bytes)?;
        let desc = self.init.call(&mut self.store, (ptr, len))?;
        Ok(serde_json::from_slice(&self.read_desc(desc)?)?)
    }

    fn call_step(&mut self, event: &Event) -> anyhow::Result<Command> {
        let bytes = serde_json::to_vec(event)?;
        let (ptr, len) = self.write(&bytes)?;
        let desc = self.step.call(&mut self.store, (ptr, len))?;
        Ok(serde_json::from_slice(&self.read_desc(desc)?)?)
    }

    /// Ask the guest whether generation should continue.
    fn call_on_token(&mut self, token: &str) -> anyhow::Result<bool> {
        // Cloned rather than moved: wasmtime's TypedFunc is Clone but not Copy,
        // and cloning also ends the borrow of `self` before `write` needs it
        // mutably.
        let Some(f) = self.on_token.clone() else {
            return Ok(true);
        };
        let (ptr, len) = self.write(token.as_bytes())?;
        Ok(f.call(&mut self.store, (ptr, len))? == 0)
    }
}

/// Read a block's declared signature out of a compiled module.
///
/// Instantiates the module and calls its `cf_signature` export. That is heavier
/// than parsing a sidecar file, and it is the point: the answer comes from the
/// artifact that will actually run, so it cannot describe a different version of
/// the block than the one being checked.
///
/// A block built before signatures existed has no such export. That is not an
/// error — it reports the permissive default, so an older block still composes,
/// just without the seam being checked.
pub fn read_signature(
    engine: &Engine,
    module_bytes: &[u8],
) -> anyhow::Result<cuttlefish_abi::Signature> {
    let permissive = cuttlefish_abi::Signature {
        input: cuttlefish_abi::Ty::Json,
        output: cuttlefish_abi::Ty::Json,
    };

    // Deliberately does not go through `Guest`, which requires the whole reactor
    // — alloc, init, step. Reading a declaration should not demand that a module
    // be runnable: a block missing an export has a real problem, but it is one
    // worth reporting when the job runs, with the job's error handling, rather
    // than as a confusing failure during a typecheck.
    let module = Module::new(engine, module_bytes)?;
    let linker: Linker<()> = Linker::new(engine);
    let mut store = Store::new(engine, ());
    let instance = linker.instantiate(&mut store, &module)?;

    let Ok(signature) = instance.get_typed_func::<(), u32>(&mut store, "cf_signature") else {
        return Ok(permissive);
    };
    let Some(memory) = instance.get_memory(&mut store, "memory") else {
        return Ok(permissive);
    };

    let desc_ptr = signature.call(&mut store, ())? as usize;
    let mut desc = [0u8; 8];
    memory.read(&mut store, desc_ptr, &mut desc)?;
    let ptr = u32::from_le_bytes(desc[..4].try_into().expect("4 bytes")) as usize;
    let len = u32::from_le_bytes(desc[4..].try_into().expect("4 bytes")) as usize;

    let mut buf = vec![0u8; len];
    memory.read(&mut store, ptr, &mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

fn fail(code: &str, message: impl Into<String>, usage: Usage) -> Envelope {
    Envelope {
        status: JobStatus::Failed,
        result: None,
        error: Some(JobError {
            code: code.into(),
            message: message.into(),
        }),
        usage,
    }
}

/// Whether an `InputExpr` (transitively) references any node in `skipped`.
fn references_any(
    expr: &cuttlefish_core::graph::InputExpr,
    skipped: &std::collections::HashSet<String>,
) -> bool {
    use cuttlefish_core::graph::InputExpr;
    match expr {
        InputExpr::FromNode(n) => skipped.contains(n),
        InputExpr::Record(fields) => fields.values().any(|e| references_any(e, skipped)),
        InputExpr::List(items) => items.iter().any(|e| references_any(e, skipped)),
    }
}

/// Compose an `InputExpr` into an actual JSON value, by looking up each
/// referenced node's already-produced output. Mirrors `dag::evaluate_expr_ty`
/// (which does the same composition at the *type* level, at check time) —
/// this is the runtime analogue.
fn evaluate_input(
    expr: &cuttlefish_core::graph::InputExpr,
    outputs: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    use cuttlefish_core::graph::InputExpr;
    match expr {
        InputExpr::FromNode(n) => outputs.get(n).cloned().unwrap_or(serde_json::Value::Null),
        InputExpr::Record(fields) => {
            let mut map = serde_json::Map::new();
            for (k, v) in fields {
                map.insert(k.clone(), evaluate_input(v, outputs));
            }
            serde_json::Value::Object(map)
        }
        InputExpr::List(items) => {
            serde_json::Value::Array(items.iter().map(|e| evaluate_input(e, outputs)).collect())
        }
    }
}

fn cancelled(usage: Usage, message: &str) -> Envelope {
    Envelope {
        status: JobStatus::Cancelled,
        result: None,
        error: Some(JobError {
            code: error_codes::CANCELLED.into(),
            message: message.into(),
        }),
        usage,
    }
}

/// Drive one job to completion.
///
/// Always returns an [`Envelope`]; failures are values, not errors, because the
/// caller has to report *something* to whoever submitted the job.
pub async fn run_job(
    engine: Arc<Engine>,
    backend: Arc<dyn InferBackend>,
    job: JobSpec,
    events: mpsc::Sender<JobEvent>,
    cancel: CancellationToken,
) -> Envelope {
    let started = Instant::now();
    let mut usage = Usage {
        model: backend.model_name(),
        ..Usage::default()
    };

    // Dropped when this function returns, closing every file the job opened.
    // That job-scoped lifetime is what makes handles unforgeable across jobs.
    //
    // Shared across stages on purpose: a handle produced by one block — a
    // rendered page, say — stays usable by the next. Confining it to one stage
    // would make a pipeline strictly weaker than a single block that did the
    // same work, while adding nothing, since the job boundary is what the
    // security property rests on.
    let mut handles = Handles::default();

    if job.nodes.is_empty() {
        usage.duration_ms = started.elapsed().as_millis() as u64;
        return fail(
            error_codes::SCHEMA_VALIDATION_FAILED,
            "this job has no nodes to run",
            usage,
        );
    }

    let total = job.nodes.len();
    let mut outputs: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    let mut skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut route_taken: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for (index, node) in job.nodes.iter().enumerate() {
        // Tell watchers which node is running. A pipeline that stalls is much
        // easier to diagnose when the stream says where.
        if total > 1 {
            let _ = events
                .send(JobEvent::Progress(serde_json::json!({
                    "stage": index + 1,
                    "of": total,
                    "node": node.name,
                })))
                .await;
        }

        // Branch-skip: this node is exclusive to a decision+label, and that
        // decision's chosen route (already recorded earlier in this same
        // loop, since the branching node is always topologically before the
        // nodes exclusive to its labels) doesn't match.
        if let Some(ex) = job.exclusive_to.get(&node.name) {
            if let Some(taken) = route_taken.get(&ex.decision) {
                if taken != &ex.label {
                    skipped.insert(node.name.clone());
                    continue;
                }
            }
        }
        // Transitive skip: this node's input needs a skipped node's output.
        if let Some(expr) = &node.input {
            if references_any(expr, &skipped) {
                skipped.insert(node.name.clone());
                continue;
            }
        }

        let node_input = match &node.input {
            None => job.input.clone(),
            Some(expr) => evaluate_input(expr, &outputs),
        };

        // Bounded loop: repeat_until re-invokes this node on its own prior
        // output, up to max_iterations. A node without repeat_until runs
        // exactly once (the loop's `None` arm breaks immediately).
        let mut current_input = node_input;
        let mut iterations: u32 = 0;
        let result = loop {
            let r = match run_stage(
                &engine,
                &backend,
                &node.module_bytes,
                current_input.clone(),
                &job.caps,
                &mut handles,
                &events,
                &cancel,
                &mut usage,
                started,
                index,
            )
            .await
            {
                Ok(v) => v,
                Err(envelope) => return envelope,
            };
            match &node.repeat_until {
                None => break r,
                Some(field) => {
                    iterations += 1;
                    let done = r.get(field).and_then(|v| v.as_str()) == Some("done");
                    if done {
                        break r;
                    }
                    let max = node
                        .max_iterations
                        .expect("repeat_until requires max_iterations, enforced at parse time");
                    if iterations >= max {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(
                            error_codes::SCHEMA_VALIDATION_FAILED,
                            format!(
                                "node `{}` did not reach repeat_until=\"done\" within max_iterations={max}",
                                node.name
                            ),
                            usage,
                        );
                    }
                    current_input = r;
                }
            }
        };

        // A branching node: read its `route` field and record the decision
        // for later nodes' branch-skip check (top of this loop).
        //
        // Whether this node is actually a `branches` decision at all is
        // determined from `job.exclusive_to` rather than by threading
        // `Branches` itself into `JobSpec`: `dag::compute_branch_exclusivity`
        // seeds an entry for every `(label, target)` pair of every declared
        // decision, so the set of `label`s across all `exclusive_to` entries
        // whose `decision` names this node IS exactly this decision's full,
        // valid label set. An unmatched or missing `route` on a genuine
        // decision node is a job failure — loud, not a silent no-op — per
        // the design spec's "Conditional dispatch" section.
        let valid_labels: Vec<&str> = job
            .exclusive_to
            .values()
            .filter(|ex| ex.decision == node.name)
            .map(|ex| ex.label.as_str())
            .collect();
        if !valid_labels.is_empty() {
            match result.get("route").and_then(|v| v.as_str()) {
                Some(route) if valid_labels.contains(&route) => {
                    route_taken.insert(node.name.clone(), route.to_string());
                }
                Some(route) => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::SCHEMA_VALIDATION_FAILED,
                        format!(
                            "node `{}` produced route \"{route}\", which doesn't match any \
                             declared label for this branches decision",
                            node.name
                        ),
                        usage,
                    );
                }
                None => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::SCHEMA_VALIDATION_FAILED,
                        format!(
                            "node `{}` is a branches decision but its output has no `route` field",
                            node.name
                        ),
                        usage,
                    );
                }
            }
        } else if let Some(route) = result.get("route").and_then(|v| v.as_str()) {
            // Not a declared decision node, but its output happens to carry a
            // route field anyway — harmless, record it defensively (no
            // downstream node's exclusive_to can reference this decision
            // name if it isn't a real decision, so this is inert either way,
            // just avoids silently dropping information that happens to be
            // present).
            route_taken.insert(node.name.clone(), route.to_string());
        }

        outputs.insert(node.name.clone(), result);
    }

    usage.duration_ms = started.elapsed().as_millis() as u64;

    // What is "the" job result for a graph? Convention: the output of the
    // LAST node in topological order that wasn't skipped. This exactly
    // matches today's linear-pipeline behavior in the degenerate case (a
    // linear chain's last node in topo order is its sole sink), and
    // generalizes sensibly to a branching graph (exactly one path executes,
    // so there's still a well-defined "last node that actually ran") and to
    // a fan-in graph without branches (the join/sink node is last in topo
    // order, since nothing depends on it).
    //
    // Known limitation: for a graph with two or more independent sinks (no
    // edge between them at all — `cuttlefishd`'s daemon path genuinely
    // accepts such graphs, unlike `cuttlefish build`, which restricts itself
    // to linear graphs), "last in topological order" is decided by the
    // topological sort's tie-break (alphabetical node name among ready
    // nodes), not by any deliberate semantic answer about which sink's
    // output should represent the job. Don't mistake that tie-break for a
    // considered multi-sink policy — it isn't one.
    let result = job
        .nodes
        .iter()
        .rev()
        .find_map(|n| outputs.get(&n.name).cloned());

    match result {
        Some(value) => Envelope {
            status: JobStatus::Completed,
            result: Some(value),
            error: None,
            usage,
        },
        None => fail(
            error_codes::SCHEMA_VALIDATION_FAILED,
            "every node in this job was skipped; there is no result",
            usage,
        ),
    }
}

/// Run one block to completion, returning what it produced.
///
/// `Err` carries a finished [`Envelope`]: a stage that fails ends the whole job,
/// because a later stage's input is the earlier one's output and there is
/// nothing sensible to feed it.
#[allow(clippy::too_many_arguments)]
async fn run_stage(
    engine: &Engine,
    backend: &Arc<dyn InferBackend>,
    module_bytes: &[u8],
    input: serde_json::Value,
    caps: &Capabilities,
    handles: &mut Handles,
    events: &mpsc::Sender<JobEvent>,
    cancel: &CancellationToken,
    usage: &mut Usage,
    started: Instant,
    stage_index: usize,
) -> Result<serde_json::Value, Envelope> {
    // Naming the stage turns "the job failed" into "the second block failed",
    // which is the difference between a usable error and a hunt.
    let blame = |message: String| -> String {
        if stage_index == 0 {
            message
        } else {
            format!("block {} of the pipeline: {message}", stage_index + 1)
        }
    };
    // Documents are read from their path rather than their descriptor — both
    // extraction and rendering want a file. Kept beside the handle table so the
    // two are dropped together at the end of the job.
    let mut doc_paths: std::collections::HashMap<u32, std::path::PathBuf> =
        std::collections::HashMap::new();

    let mut guest = match Guest::new(engine, module_bytes) {
        Ok(g) => g,
        Err(e) => {
            return Err(fail(
                error_codes::WASM_TRAP,
                blame(e.to_string()),
                usage.clone(),
            ))
        }
    };

    let mut command = match guest.call_init(&input) {
        Ok(c) => c,
        Err(e) => {
            return Err(fail(
                error_codes::WASM_TRAP,
                blame(e.to_string()),
                usage.clone(),
            ))
        }
    };

    loop {
        if cancel.is_cancelled() {
            usage.duration_ms = started.elapsed().as_millis() as u64;
            return Err(cancelled(usage.clone(), "job cancelled"));
        }

        let event = match command {
            // Ends this stage, not the job: the value becomes the next
            // block's input, or the job's result if this was the last.
            Command::Done { result } => return Ok(result),
            Command::Fail { code, message } => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return Err(fail(&code, blame(message), usage.clone()));
            }
            Command::Emit { progress } => {
                let _ = events.send(JobEvent::Progress(progress)).await;
                Event::Emitted
            }

            // The capability check lives here, at Open, and nowhere else. Slice
            // takes a handle rather than a path, and handles are job-scoped, so
            // there is no second place a path can enter the system.
            Command::Open { path } => {
                let p = std::path::PathBuf::from(&path);
                if !caps.allows_read(&p) {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("read not permitted: {path}"),
                        usage.clone(),
                    ));
                }
                match handles.open(&p) {
                    Ok((handle, len, kind)) => {
                        // A PDF's page count and text layer need the whole file,
                        // which the handle layer deliberately does not read. Ask
                        // the document layer, and fall back to the plain kind if
                        // it cannot answer — a malformed PDF is still a file a
                        // block may want to read bytes from.
                        let kind = match kind {
                            cuttlefish_abi::MediaKind::Document { .. } => {
                                match crate::documents::inspect(&p) {
                                    Ok(info) => cuttlefish_abi::MediaKind::Document {
                                        pages: info.pages,
                                        has_text_layer: info.has_text_layer,
                                    },
                                    Err(_) => cuttlefish_abi::MediaKind::Binary,
                                }
                            }
                            other => other,
                        };
                        // Remember the path: rendering and text extraction work
                        // from a file, not from the open descriptor.
                        doc_paths.insert(handle, p.clone());
                        Event::Opened { handle, len, kind }
                    }
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(
                            error_codes::CAPABILITY_DENIED,
                            e.to_string(),
                            usage.clone(),
                        ));
                    }
                }
            }

            Command::Slice {
                handle,
                offset,
                len,
            } => match handles.slice(handle, offset, len) {
                Ok(w) => Event::Sliced {
                    text: w.text,
                    next_offset: w.next_offset,
                },
                Err(e) => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        e.to_string(),
                        usage.clone(),
                    ));
                }
            },

            Command::SliceBytes {
                handle,
                offset,
                len,
            } => match handles.slice_bytes(handle, offset, len) {
                Ok((bytes, next_offset)) => {
                    use base64::Engine;
                    Event::SlicedBytes {
                        bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        next_offset,
                    }
                }
                Err(e) => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        e.to_string(),
                        usage.clone(),
                    ));
                }
            },

            Command::PageText { handle, page } => {
                let Some(path) = doc_paths.get(&handle).cloned() else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("no such handle: {handle}"),
                        usage.clone(),
                    ));
                };
                match crate::documents::page_text(&path, page) {
                    Ok(text) => Event::PageTexted { text },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                }
            }

            Command::PageImage { handle, page } => {
                let Some(path) = doc_paths.get(&handle).cloned() else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("no such handle: {handle}"),
                        usage.clone(),
                    ));
                };
                // A rendered page becomes a handle like any other, so it can be
                // named in Infer exactly as a file-backed image would be.
                match crate::documents::render_page(&path, page, RENDER_WIDTH) {
                    Ok(png) => {
                        let (handle, len) = handles.insert_bytes(
                            png,
                            cuttlefish_abi::MediaKind::Image {
                                format: "png".into(),
                            },
                        );
                        Event::PageImaged { handle, len }
                    }
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                }
            }

            Command::Infer {
                prompt,
                max_tokens,
                images,
            } => {
                // Images are named by handle; the host loads the bytes, so they
                // never pass through guest memory.
                // Refuse rather than drop. Sending images to a backend that
                // cannot use them produces a confident answer about nothing,
                // which reads as a bad model rather than a misconfigured job —
                // and the caller has no way to tell the difference.
                if !images.is_empty() && !backend.supports_images() {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::UNSUPPORTED,
                        format!(
                            "this job supplied {} image(s), but the backend serving `{}` cannot \
                             accept them. Use a vision-capable model through the `ollama` \
                             provider, or change the block to send text only.",
                            images.len(),
                            backend.model_name()
                        ),
                        usage.clone(),
                    ));
                }
                let mut image_bytes = Vec::with_capacity(images.len());
                for handle in &images {
                    match handles.read_all(*handle) {
                        Ok(bytes) => image_bytes.push(bytes),
                        Err(e) => {
                            usage.duration_ms = started.elapsed().as_millis() as u64;
                            return Err(fail(
                                error_codes::CAPABILITY_DENIED,
                                e.to_string(),
                                usage.clone(),
                            ));
                        }
                    }
                }

                // Tokens must reach the guest *while* generation runs, because
                // the guest's Stop verdict is what ends it early. The wasmtime
                // Store is !Sync and cannot be touched from inside the backend's
                // callback, so a channel carries tokens out and a shared flag
                // carries the verdict back — without sharing the Store.
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                let stop = Arc::new(AtomicBool::new(false));
                let sink_stop = stop.clone();
                let mut sink = move |t: &str| {
                    tx.send(t.to_string()).is_ok() && !sink_stop.load(Ordering::Relaxed)
                };

                let mut trap: Option<String> = None;
                let outcome: Option<anyhow::Result<InferResult>> = {
                    let request = InferRequest {
                        prompt: &prompt,
                        max_tokens,
                        images: &image_bytes,
                    };
                    let infer = backend.infer(request, &mut sink);
                    tokio::pin!(infer);
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break None,
                            Some(tok) = rx.recv() => {
                                let _ = events.send(JobEvent::Token(tok.clone())).await;
                                match guest.call_on_token(&tok) {
                                    Ok(true) => {}
                                    Ok(false) => stop.store(true, Ordering::Relaxed),
                                    Err(e) => {
                                        trap = Some(e.to_string());
                                        break None;
                                    }
                                }
                            }
                            r = &mut infer => break Some(r),
                        }
                    }
                };

                if let Some(message) = trap {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(error_codes::WASM_TRAP, message, usage.clone()));
                }

                // Tokens generated in the same poll as the last one are still
                // queued; forward them so the stream is complete.
                while let Ok(tok) = rx.try_recv() {
                    let _ = events.send(JobEvent::Token(tok)).await;
                }

                match outcome {
                    None => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(cancelled(usage.clone(), "cancelled during inference"));
                    }
                    Some(Err(e)) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(
                            error_codes::MODEL_LOAD_FAILED,
                            e.to_string(),
                            usage.clone(),
                        ));
                    }
                    Some(Ok(r)) => {
                        usage.tokens_in += r.tokens_in;
                        usage.tokens_out += r.tokens_out;
                        Event::InferDone {
                            text: r.text,
                            tokens_out: r.tokens_out,
                        }
                    }
                }
            }
        };

        command = match guest.call_step(&event) {
            Ok(c) => c,
            Err(e) => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return Err(fail(
                    error_codes::WASM_TRAP,
                    blame(e.to_string()),
                    usage.clone(),
                ));
            }
        };
    }
}
