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

/// Backends for models *other than* the job's own, keyed by the model that
/// names them.
///
/// Named `alternates` rather than `backends` on purpose: `run_job` already
/// takes the job's own backend, and a field called `backends` sitting beside
/// a parameter called `backend` is a trap. Everything here is something the
/// job reaches only by explicit instruction — an `on_fail = [ reroute ... ]`
/// rung, or a `Judge` that names its own model.
///
/// Resolved once at daemon startup, so a model this build cannot serve fails
/// as the daemon comes up rather than three hours into a campaign, at the
/// exact moment something has already gone wrong.
pub type Alternates =
    std::collections::HashMap<cuttlefish_core::spec::ModelRef, Arc<dyn InferBackend>>;

/// Every model a spec can reach *besides* its own — the `reroute` targets
/// and the model-bearing `Judge`s, deduplicated.
///
/// Exists so the daemon can resolve them all at startup. Collecting them
/// here rather than in `cuttlefishd` keeps the knowledge of which node
/// fields name a model next to the type that consumes them, so adding a
/// third such field later is one edit rather than two.
pub fn alternate_models_of(
    spec: &cuttlefish_core::spec::Spec,
) -> Vec<cuttlefish_core::spec::ModelRef> {
    use cuttlefish_core::graph::{AcceptCheck, Rung};
    let mut seen: Vec<cuttlefish_core::spec::ModelRef> = Vec::new();
    let mut push = |model: cuttlefish_core::spec::ModelRef| {
        // The job's own model is not an alternate; it already has a backend.
        if model != spec.model && !seen.contains(&model) {
            seen.push(model);
        }
    };
    for (_, node) in &spec.nodes.nodes {
        for rung in &node.on_fail {
            if let Rung::Reroute(model) = rung {
                push(model.clone());
            }
        }
        for check in &node.accept {
            if let AcceptCheck::Judge {
                model: Some(model), ..
            } = check
            {
                push(model.clone());
            }
        }
    }
    seen
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
    /// The backend serving `embed`, if the spec declared an
    /// `embedding_model`. Resolved at startup like every other model, so a
    /// spec naming one this build cannot serve fails as the daemon comes up
    /// rather than partway through a corpus.
    pub embedder: Option<Arc<dyn InferBackend>>,
    /// Backends for models beyond the job's own — see [`Alternates`].
    /// Empty for a spec with no `reroute` rung and no model-bearing `Judge`,
    /// which is every spec that existed before acceptance contracts.
    pub alternates: Alternates,
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
    fn new(
        engine: &Engine,
        cache: &crate::module_cache::ModuleCache,
        module_bytes: &[u8],
    ) -> anyhow::Result<Self> {
        let module = cache.compile(engine, module_bytes)?;

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
///
/// `ledger` is consulted before any resume-sensitive decision (branch-skip,
/// transitive-skip, or actually running a node) and written to immediately
/// after that decision is made, so that a process restart mid-job — a later
/// task's concern, not this function's — can resume from exactly the state
/// this function left behind. The whole body runs inside a single labeled
/// block (`'run: { ... }`) so that every exit path, however it got there,
/// still reaches the one `ledger.finish(...)` call at the end.
pub async fn run_job(
    engine: Arc<Engine>,
    backend: Arc<dyn InferBackend>,
    job: JobSpec,
    events: mpsc::Sender<JobEvent>,
    cancel: CancellationToken,
    ledger: &crate::ledger::Ledger,
    cache: &crate::module_cache::ModuleCache,
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

    let envelope = 'run: {
        if job.nodes.is_empty() {
            usage.duration_ms = started.elapsed().as_millis() as u64;
            break 'run fail(
                error_codes::SCHEMA_VALIDATION_FAILED,
                "this job has no nodes to run",
                usage,
            );
        }

        // Compile every node's `accept` list up front, indexed by node
        // position. A malformed schema is a property of the spec, so it
        // should stop the job at the start rather than surface as a bizarre
        // acceptance failure once half a campaign has already run.
        let checks: Vec<crate::accept::CompiledChecks> = match job
            .nodes
            .iter()
            .map(|n| {
                crate::accept::CompiledChecks::compile(&n.accept)
                    .map_err(|e| format!("node `{}`: {e}", n.name))
            })
            .collect()
        {
            Ok(c) => c,
            Err(message) => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                break 'run fail(error_codes::SCHEMA_VALIDATION_FAILED, message, usage);
            }
        };

        let total = job.nodes.len();
        let mut outputs: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        let mut skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut route_taken: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for (index, node) in job.nodes.iter().enumerate() {
            // Resume: a node the ledger already marked skipped stays skipped
            // — its branch decision is not re-evaluated.
            match ledger.is_skipped(&node.name) {
                Ok(true) => {
                    skipped.insert(node.name.clone());
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    break 'run fail(
                        error_codes::SCHEMA_VALIDATION_FAILED,
                        format!("reading ledger skip state for node `{}`: {e}", node.name),
                        usage,
                    );
                }
            }

            // Resume: a completed checkpoint means reuse the cached output
            // instead of re-running.
            match ledger.get_completed(&node.name) {
                Ok(Some(cached)) => {
                    // If this was a branches decision node, its route must be
                    // recorded in `route_taken` here too — not just after a
                    // fresh `run_stage` call below — or a downstream,
                    // not-yet-reached branch-exclusive node would see no
                    // recorded decision at all on a resumed run and
                    // (incorrectly) never be skipped. The checkpoint only
                    // ever holds a value that already passed this same route
                    // validation the first time it ran, so this is purely
                    // re-deriving `route_taken`, never re-validating.
                    if let Some(route) = cached.get("route").and_then(|v| v.as_str()) {
                        route_taken.insert(node.name.clone(), route.to_string());
                    }
                    outputs.insert(node.name.clone(), cached);
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    break 'run fail(
                        error_codes::SCHEMA_VALIDATION_FAILED,
                        format!("reading ledger checkpoint for node `{}`: {e}", node.name),
                        usage,
                    );
                }
            }

            // Tell watchers which node is running. A pipeline that stalls is
            // much easier to diagnose when the stream says where.
            if total > 1 {
                let _ = events
                    .send(JobEvent::Progress(serde_json::json!({
                        "stage": index + 1,
                        "of": total,
                        "node": node.name,
                    })))
                    .await;
            }

            // Fresh branch-skip decision (only reached if the ledger had no
            // recorded state for this node — i.e. this is either a fresh
            // run, or a resumed run that hadn't reached this node yet).
            //
            // Branch-skip: this node is exclusive to a decision+label, and
            // that decision's chosen route (already recorded earlier in this
            // same loop, since the branching node is always topologically
            // before the nodes exclusive to its labels) doesn't match.
            if let Some(ex) = job.exclusive_to.get(&node.name) {
                if let Some(taken) = route_taken.get(&ex.decision) {
                    if taken != &ex.label {
                        skipped.insert(node.name.clone());
                        if let Err(e) = ledger.write_skipped(&node.name) {
                            usage.duration_ms = started.elapsed().as_millis() as u64;
                            break 'run fail(
                                error_codes::SCHEMA_VALIDATION_FAILED,
                                format!("recording skip for node `{}` in ledger: {e}", node.name),
                                usage,
                            );
                        }
                        continue;
                    }
                }
            }
            // Transitive skip: this node's input needs a skipped node's
            // output.
            if let Some(expr) = &node.input {
                if references_any(expr, &skipped) {
                    skipped.insert(node.name.clone());
                    if let Err(e) = ledger.write_skipped(&node.name) {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        break 'run fail(
                            error_codes::SCHEMA_VALIDATION_FAILED,
                            format!("recording skip for node `{}` in ledger: {e}", node.name),
                            usage,
                        );
                    }
                    continue;
                }
            }

            let node_input = match &node.input {
                None => job.input.clone(),
                Some(expr) => evaluate_input(expr, &outputs),
            };

            // Fan-out: this node runs once per manifest line rather than
            // once, with each item checkpointed independently. Handled by
            // its own helper because it is a genuinely different execution
            // shape, not a variation on the repeat_until loop below (which
            // it is parse-time forbidden to combine with).
            if node.over.is_some() {
                let collected = match run_fanout_node(
                    &engine,
                    cache,
                    &backend,
                    &job.alternates,
                    &checks[index],
                    node,
                    &job.caps,
                    job.embedder.as_ref(),
                    &mut handles,
                    &events,
                    &cancel,
                    &mut usage,
                    started,
                    index,
                    total,
                    ledger,
                )
                .await
                {
                    Ok(v) => v,
                    Err(envelope) => break 'run envelope,
                };
                if let Err(e) = ledger.write_completed(&node.name, &collected) {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    break 'run fail(
                        error_codes::SCHEMA_VALIDATION_FAILED,
                        format!(
                            "recording checkpoint for node `{}` in ledger: {e}",
                            node.name
                        ),
                        usage,
                    );
                }
                outputs.insert(node.name.clone(), collected);
                continue;
            }

            // Run the node, holding its output to the declared type and to
            // every `accept` check, and climbing `on_fail` if it isn't
            // accepted. The bounded `repeat_until` loop lives inside one
            // attempt — see `Ladder`.
            //
            // Nothing checked a block's *actual* output against what it
            // declared until this existed: a Script or Rust block could
            // claim `{summary: text}` and return `{text: "..."}` and the
            // job would still complete "successfully", silently handing
            // whatever consumed `summary` a `null`. `matches_value` is
            // deliberately permissive about `bytes`/`image`/`document` (see
            // its own doc comment) so this only ever catches genuine shape
            // mismatches, not false positives on types this protocol has no
            // fixed JSON encoding for.
            let job_dir = ledger.job_dir();
            let ladder = Ladder {
                engine: &engine,
                embedder: job.embedder.as_ref(),
                job_dir,
                cache,
                default_backend: &backend,
                alternates: &job.alternates,
                checks: &checks[index],
                expected: &node.signature.output,
                on_fail: &node.on_fail,
                repeat_until: node.repeat_until.as_deref(),
                max_iterations: node.max_iterations,
                caps: &job.caps,
                events: &events,
                cancel: &cancel,
                started,
                index,
            };
            let result = match ladder
                .run(
                    &node.module_bytes,
                    node.script.as_deref(),
                    node_input.clone(),
                    &mut handles,
                    &mut usage,
                )
                .await
            {
                Ok(value) => value,
                Err(LadderError::Fatal(envelope)) => break 'run *envelope,
                Err(LadderError::Exhausted {
                    reason,
                    escalated,
                    envelope,
                }) => {
                    // Record the give-up *before* failing the job. An
                    // escalation that only exists in the returned envelope is
                    // gone the moment the caller stops looking, which defeats
                    // the point: the whole reason to escalate is that nobody
                    // is watching right now.
                    if escalated {
                        // With the input, so the escalation is drainable:
                        // "here is exactly what this node was handed when it
                        // gave up" is what reproducing it requires.
                        if let Err(e) =
                            ledger.write_escalated(&node.name, None, &reason, Some(&node_input))
                        {
                            eprintln!("recording escalation for node `{}`: {e}", node.name);
                        }
                    }
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    // The block's own envelope when it has one, so a
                    // `wasm_trap` still reads as a `wasm_trap`.
                    break 'run match envelope {
                        Some(e) => *e,
                        None => fail(
                            error_codes::SCHEMA_VALIDATION_FAILED,
                            format!("node `{}`: {reason}", node.name),
                            usage,
                        ),
                    };
                }
            };

            // A branching node: read its `route` field and record the
            // decision for later nodes' branch-skip check (top of this
            // loop).
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
                        break 'run fail(
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
                        break 'run fail(
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
                // Not a declared decision node, but its output happens to
                // carry a route field anyway — harmless, record it
                // defensively (no downstream node's exclusive_to can
                // reference this decision name if it isn't a real decision,
                // so this is inert either way, just avoids silently
                // dropping information that happens to be present).
                route_taken.insert(node.name.clone(), route.to_string());
            }

            // Checkpoint a genuinely-executed node's result before recording
            // it in `outputs` — the ledger write must happen before the
            // in-memory outputs map update so a crash between the two can't
            // leave the ledger silently behind the in-memory state (the
            // in-memory state doesn't survive a crash anyway, so
            // ledger-first is the only order that matters for durability;
            // the reverse order would just be a smaller window for the same
            // underlying property, not correctness).
            if let Err(e) = ledger.write_completed(&node.name, &result) {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                break 'run fail(
                    error_codes::SCHEMA_VALIDATION_FAILED,
                    format!(
                        "recording checkpoint for node `{}` in ledger: {e}",
                        node.name
                    ),
                    usage,
                );
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
    };

    let ledger_status = match envelope.status {
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
        _ => "running", // shouldn't occur — defensive only
    };
    if let Err(e) = ledger.finish(ledger_status) {
        // A ledger write failure at this final step doesn't invalidate the
        // job's already-computed, correct result — losing it here means a
        // wrong Interrupted-detection on next restart, not a wrong result
        // now. Loud enough to notice, not loud enough to discard real work
        // that already succeeded or failed for its own, unrelated reason.
        eprintln!("warning: failed to record job {ledger_status} status in ledger: {e}");
    }
    envelope
}

/// Run one block to completion, returning what it produced.
///
/// `Err` carries a finished [`Envelope`]: a stage that fails ends the whole job,
/// because a later stage's input is the earlier one's output and there is
/// nothing sensible to feed it.
/// Run one fan-out node: its block once per manifest line, each item
/// checkpointed independently, then the results materialized for whatever
/// consumes them downstream.
///
/// Returns the collection record described by
/// [`crate::dag::fanout_collection_ty`] — deliberately not any one item's
/// result, since downstream consumes all of them.
///
/// # Why the ledger is authoritative and the files are a projection
///
/// Items are recorded in SQLite as they conclude, and `results.jsonl` /
/// `failures.jsonl` are written once at the end by reading those rows back.
/// Appending to the text files as items finished would be simpler, but a
/// crash mid-append leaves a torn final line that resume then has to
/// reconcile against the ledger. Projecting at the end makes that class of
/// bug unrepresentable: SQLite is already transactional, so let it be the
/// thing that's true.
#[allow(clippy::too_many_arguments)]
async fn run_fanout_node(
    engine: &Engine,
    cache: &crate::module_cache::ModuleCache,
    backend: &Arc<dyn InferBackend>,
    alternates: &Alternates,
    checks: &crate::accept::CompiledChecks,
    node: &crate::dag::CheckedNode,
    caps: &Capabilities,
    embedder: Option<&Arc<dyn InferBackend>>,
    handles: &mut Handles,
    events: &mpsc::Sender<JobEvent>,
    cancel: &CancellationToken,
    usage: &mut Usage,
    started: Instant,
    index: usize,
    total: usize,
    ledger: &crate::ledger::Ledger,
) -> Result<serde_json::Value, Envelope> {
    use sha2::{Digest, Sha256};

    let manifest_path = node
        .over
        .as_ref()
        .expect("run_fanout_node is only called for a node with `over`");
    let node_name = node.name.as_str();
    // NOT `node.signature.output` — that has already been replaced with the
    // collection record for downstream typing, so validating an item against
    // it would reject every single item.
    let item_output = node
        .item_output
        .as_ref()
        .unwrap_or(&node.signature.output)
        .clone();

    let bail = |message: String, usage: &mut Usage| -> Envelope {
        usage.duration_ms = started.elapsed().as_millis() as u64;
        fail(
            error_codes::SCHEMA_VALIDATION_FAILED,
            message,
            usage.clone(),
        )
    };

    // --- Read and validate the manifest up front -------------------------
    //
    // A malformed manifest is an authoring error, so it fails the whole job
    // before any item runs, rather than surfacing as N mysterious item
    // failures partway through.
    let bytes = std::fs::read(manifest_path).map_err(|e| {
        bail(
            format!(
                "node `{node_name}`: reading fan-out manifest {}: {e}",
                manifest_path.display()
            ),
            usage,
        )
    })?;

    let text = String::from_utf8(bytes.clone()).map_err(|e| {
        bail(
            format!(
                "node `{node_name}`: fan-out manifest {} is not valid UTF-8: {e}",
                manifest_path.display()
            ),
            usage,
        )
    })?;

    let mut items: Vec<serde_json::Value> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(v) => items.push(v),
            Err(e) => {
                return Err(bail(
                    format!(
                        "node `{node_name}`: fan-out manifest {} line {} is not valid JSON: {e}",
                        manifest_path.display(),
                        i + 1
                    ),
                    usage,
                ))
            }
        }
    }

    if items.is_empty() {
        return Err(bail(
            format!(
                "node `{node_name}`: fan-out manifest {} is empty — zero items almost always \
                 means the step that produced it failed, and reducing over nothing would \
                 silently look like success",
                manifest_path.display()
            ),
            usage,
        ));
    }

    // --- Pin this node to the manifest it ran against --------------------
    //
    // An item_index is only meaningful relative to one manifest, and no
    // graph fingerprint can see an edit to the manifest *file*.
    let digest = crate::hex::encode(Sha256::digest(&bytes));
    match ledger.check_or_record_manifest(node_name, &digest, items.len()) {
        Ok(Ok(())) => {}
        Ok(Err(previous)) => {
            return Err(bail(
                format!(
                    "node `{node_name}`: fan-out manifest {} has changed since this job first \
                     ran (was {previous}, now {digest}) — recorded item indices no longer refer \
                     to the same inputs, so resuming would pair results with the wrong items; \
                     re-submit the job instead",
                    manifest_path.display()
                ),
                usage,
            ))
        }
        Err(e) => {
            return Err(bail(
                format!("node `{node_name}`: recording fan-out manifest digest: {e}"),
                usage,
            ))
        }
    }

    // --- Run each item ---------------------------------------------------
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for (item_index, item_input) in items.iter().enumerate() {
        // Cancellation is checked here, not only inside run_stage: without
        // it a cancelled 500-item campaign would keep starting new items,
        // finishing only once the manifest ran out.
        if cancel.is_cancelled() {
            usage.duration_ms = started.elapsed().as_millis() as u64;
            return Err(Envelope {
                status: JobStatus::Cancelled,
                result: None,
                error: None,
                usage: usage.clone(),
            });
        }

        // Resume: an item that already concluded — either way — is not run
        // again. An item that was merely in flight when a previous run died
        // left no row, so it lands here and runs now.
        match ledger.item_concluded(node_name, item_index) {
            Ok(true) => {
                if ledger
                    .get_item_completed(node_name, item_index)
                    .unwrap_or(None)
                    .is_some()
                {
                    succeeded += 1;
                } else {
                    failed += 1;
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                return Err(bail(
                    format!("node `{node_name}`: reading item {item_index} from ledger: {e}"),
                    usage,
                ))
            }
        }

        let _ = events
            .send(JobEvent::Progress(serde_json::json!({
                "stage": index + 1,
                "of": total,
                "node": node_name,
                "item": item_index,
                "items": items.len(),
                "succeeded": succeeded,
                "failed": failed,
            })))
            .await;

        // Each item's input must satisfy what the block declared. A single
        // mismatched line is a data-quality problem, not an authoring one,
        // so it fails that item and the run continues.
        if !node.signature.input.matches_value(item_input) {
            failed += 1;
            let message = format!(
                "item {item_index} does not match the block's declared input `{}`",
                node.signature.input
            );
            if let Err(e) =
                ledger.write_item_failed(node_name, item_index, &message, Some(item_input))
            {
                return Err(bail(
                    format!("node `{node_name}`: recording item {item_index} failure: {e}"),
                    usage,
                ));
            }
            continue;
        }

        // The same ladder the ordinary-node path uses, per item. `expected`
        // is the *per-item* output, and `repeat_until` is `None` because
        // combining it with `over` is forbidden at parse time.
        let job_dir = ledger.job_dir();
        let ladder = Ladder {
            engine,
            embedder,
            job_dir,
            cache,
            default_backend: backend,
            alternates,
            checks,
            expected: &item_output,
            on_fail: &node.on_fail,
            repeat_until: None,
            max_iterations: None,
            caps,
            events,
            cancel,
            started,
            index,
        };

        match ladder
            .run(
                &node.module_bytes,
                node.script.as_deref(),
                item_input.clone(),
                handles,
                usage,
            )
            .await
        {
            Ok(value) => {
                succeeded += 1;
                if let Err(e) = ledger.write_item_completed(node_name, item_index, &value) {
                    return Err(bail(
                        format!("node `{node_name}`: recording item {item_index} result: {e}"),
                        usage,
                    ));
                }
            }
            // A cancelled job must not be recorded as a per-item failure —
            // nothing about the item was wrong, and doing so would make the
            // cancellation permanent across a later resume.
            Err(LadderError::Fatal(envelope)) => return Err(*envelope),
            // A fan-out item's envelope is deliberately dropped: an item
            // failure is recorded as text and the *job* keeps going, so
            // there is nothing here for an envelope to become.
            Err(LadderError::Exhausted {
                reason, escalated, ..
            }) => {
                failed += 1;
                let message = format!("item {item_index} {reason}");
                // An escalated item is still a concluded failure — it counts
                // toward `failed` and lands in `failures.jsonl` like any
                // other. The escalation row is *additional*: it is what makes
                // this one findable later without re-reading every job.
                // Both carry the item's input, so a later drain can hand
                // the work back. Without it an escalation names an item
                // index against a manifest that may since have moved, which
                // is not enough to act on.
                let recorded = if escalated {
                    ledger.write_escalated(node_name, Some(item_index), &message, Some(item_input))
                } else {
                    ledger.write_item_failed(node_name, item_index, &message, Some(item_input))
                };
                if let Err(e) = recorded {
                    return Err(bail(
                        format!("node `{node_name}`: recording item {item_index} failure: {e}"),
                        usage,
                    ));
                }
            }
        }
    }

    if succeeded == 0 {
        // Quote the first item's actual error. "All 500 items failed" with no
        // cause is a dead end, and when every item fails the same way — which
        // is the common case — the first one is the whole story.
        let first_error = ledger
            .concluded_items(node_name)
            .ok()
            .and_then(|items| items.into_iter().find_map(|(_, _, err)| err))
            .unwrap_or_else(|| "no error recorded".to_string());
        return Err(bail(
            format!(
                "node `{node_name}`: all {failed} fan-out item(s) failed — there is nothing for \
                 a downstream node to reduce over. First failure: {first_error}"
            ),
            usage,
        ));
    }

    // --- Materialize the results from the ledger -------------------------
    //
    // Named per node: a graph may legally contain more than one fan-out
    // node, and flat results.jsonl / failures.jsonl would clobber.
    let results_dir = ledger.job_dir().join("results");
    std::fs::create_dir_all(&results_dir).map_err(|e| {
        bail(
            format!(
                "node `{node_name}`: creating {}: {e}",
                results_dir.display()
            ),
            usage,
        )
    })?;
    let results_path = results_dir.join(format!("{node_name}.results.jsonl"));
    let failures_path = results_dir.join(format!("{node_name}.failures.jsonl"));

    let concluded = ledger.concluded_items(node_name).map_err(|e| {
        bail(
            format!("node `{node_name}`: reading concluded items from ledger: {e}"),
            usage,
        )
    })?;

    let (mut results_out, mut failures_out) = (String::new(), String::new());
    for (item_index, output, error) in concluded {
        match (output, error) {
            (Some(value), _) => results_out.push_str(&format!(
                "{}\n",
                serde_json::json!({"item": item_index, "result": value})
            )),
            (None, Some(message)) => failures_out.push_str(&format!(
                "{}\n",
                serde_json::json!({"item": item_index, "error": message})
            )),
            (None, None) => {}
        }
    }

    for (path, contents) in [
        (&results_path, &results_out),
        (&failures_path, &failures_out),
    ] {
        std::fs::write(path, contents).map_err(|e| {
            bail(
                format!("node `{node_name}`: writing {}: {e}", path.display()),
                usage,
            )
        })?;
    }

    Ok(serde_json::json!({
        "results_path": results_path.to_string_lossy(),
        "failures_path": failures_path.to_string_lossy(),
        "succeeded": succeeded,
        "failed": failed,
    }))
}

/// Why an attempt ladder ran out of rungs.
enum LadderError {
    /// Every rung was climbed and the work still wasn't accepted.
    Exhausted {
        /// The last thing that went wrong, verbatim — this becomes the text
        /// a human reads in `cuttlefish escalations`, so it has to name the
        /// actual failing check rather than "acceptance failed".
        reason: String,
        /// Whether the ladder ended on an explicit `escalate` rung, as
        /// opposed to simply running out. Only the former gets an escalation
        /// row: the author asked for someone to be told.
        escalated: bool,
        /// The last attempt's own envelope, when the last thing that went
        /// wrong was the block failing rather than a check rejecting it.
        ///
        /// Carried so the error *code* survives the ladder. Without it every
        /// `wasm_trap` and `capability_denied` would reach the caller
        /// flattened into `schema_validation_failed`, which is both wrong and
        /// a silent downgrade for the many nodes that declare no `on_fail` at
        /// all and should behave exactly as they did before ladders existed.
        envelope: Option<Box<Envelope>>,
    },
    /// The job as a whole must stop — cancellation, or a host-level error
    /// that has nothing to do with this node's output being wrong. Never
    /// retried, because retrying a cancellation is how a cancelled job comes
    /// back to life.
    Fatal(Box<Envelope>),
}

/// Everything the ladder needs that doesn't change between attempts.
///
/// A struct rather than a dozen more parameters: `run_stage` already carries
/// thirteen, and the ladder wraps it from two call sites (one node, one
/// fan-out item) that must behave identically. Bundling the invariant part is
/// what makes "identically" checkable by eye.
struct Ladder<'a> {
    engine: &'a Engine,
    /// Serves `embed`, when the spec declared an embedding model.
    embedder: Option<&'a Arc<dyn InferBackend>>,
    /// Where a fetched URL is written. The job's own directory, so a
    /// download shares the job's lifetime and sits beside its results.
    job_dir: &'a std::path::Path,
    cache: &'a crate::module_cache::ModuleCache,
    /// The job's own backend: where the first attempt goes, and — always —
    /// where judges are asked. A judge run on the rerouted model would be
    /// grading its own work.
    default_backend: &'a Arc<dyn InferBackend>,
    alternates: &'a Alternates,
    checks: &'a crate::accept::CompiledChecks,
    /// What this attempt's output must structurally be. For a fan-out item
    /// this is the block's per-item output, *not* the collection the node
    /// presents downstream.
    expected: &'a cuttlefish_abi::Ty,
    on_fail: &'a [cuttlefish_core::graph::Rung],
    /// The node's bounded loop, if it has one. Held here, rather than around
    /// the ladder, because one *attempt* has to mean one whole run of the
    /// node: retrying a node that reached `max_iterations` without finishing
    /// means running its loop again from the top, not resuming it.
    repeat_until: Option<&'a str>,
    /// Required alongside `repeat_until`, enforced at parse time.
    max_iterations: Option<u32>,
    caps: &'a Capabilities,
    events: &'a mpsc::Sender<JobEvent>,
    cancel: &'a CancellationToken,
    started: Instant,
    index: usize,
}

impl Ladder<'_> {
    /// Run the work, climbing `on_fail` until something is accepted or the
    /// ladder runs out.
    ///
    /// Nothing here writes to the ledger. That is the whole point: a value
    /// that is about to be retried is not a conclusion, and recording it
    /// would make a transient rejection permanent across a resume.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        module_bytes: &[u8],
        script: Option<&str>,
        input: serde_json::Value,
        handles: &mut Handles,
        usage: &mut Usage,
    ) -> Result<serde_json::Value, LadderError> {
        use cuttlefish_core::graph::Rung;

        let mut backend = self.default_backend.clone();
        let mut rung = 0usize;
        let mut retries_left = 0u32;

        'ladder: loop {
            let (reason, envelope) = match self
                .attempt(
                    module_bytes,
                    script,
                    input.clone(),
                    &backend,
                    handles,
                    usage,
                )
                .await
            {
                Ok(value) => return Ok(value),
                Err(LadderError::Fatal(e)) => return Err(LadderError::Fatal(e)),
                Err(LadderError::Exhausted {
                    reason, envelope, ..
                }) => (reason, envelope),
            };

            // Decide the next move. This inner loop only ever advances
            // `rung`, so it cannot spin: a `Retry` sets a budget and falls
            // straight back to the attempt, and every other arm either
            // returns or re-attempts.
            loop {
                if retries_left > 0 {
                    retries_left -= 1;
                    continue 'ladder;
                }
                match self.on_fail.get(rung) {
                    None => {
                        return Err(LadderError::Exhausted {
                            reason,
                            escalated: false,
                            envelope,
                        })
                    }
                    Some(Rung::Retry(n)) => {
                        rung += 1;
                        retries_left = *n;
                    }
                    Some(Rung::Reroute(model)) => {
                        rung += 1;
                        match self.alternates.get(model) {
                            Some(b) => {
                                backend = b.clone();
                                continue 'ladder;
                            }
                            // Startup resolution should have caught this, so
                            // reaching here is a bug — but failing the node
                            // beats taking the daemon down mid-campaign, and
                            // the reason says exactly what happened.
                            None => {
                                return Err(LadderError::Exhausted {
                                    reason: format!(
                                        "reroute names model `{model}`, which was not resolved \
                                         at startup (after: {reason})"
                                    ),
                                    escalated: false,
                                    // Not the block's fault — this is an
                                    // authoring/resolution error, so it keeps
                                    // its own code rather than the last
                                    // attempt's.
                                    envelope: None,
                                });
                            }
                        }
                    }
                    Some(Rung::Escalate) => {
                        return Err(LadderError::Exhausted {
                            reason,
                            escalated: true,
                            envelope,
                        })
                    }
                }
            }
        }
    }

    /// One attempt: run the block, then hold its output to the declared type
    /// and to every `accept` check, in that order.
    ///
    /// Ordering is deliberate and cost-driven. The type check is free, a
    /// schema costs a file's worth of validation, and a judge costs a whole
    /// inference — so structurally broken output never pays for a judge, and
    /// a judge is never asked to grade something it would grade incoherently.
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        module_bytes: &[u8],
        script: Option<&str>,
        input: serde_json::Value,
        backend: &Arc<dyn InferBackend>,
        handles: &mut Handles,
        usage: &mut Usage,
    ) -> Result<serde_json::Value, LadderError> {
        // A check said no. The block itself is fine, so there is no envelope
        // to preserve.
        let rejected = |reason: String| LadderError::Exhausted {
            reason,
            escalated: false,
            envelope: None,
        };

        // The bounded loop. A node without `repeat_until` takes the `None`
        // arm on its first pass and runs exactly once.
        let mut current = input.clone();
        let mut iterations: u32 = 0;
        let value = loop {
            let produced = match run_stage(
                self.engine,
                self.cache,
                backend,
                module_bytes,
                current.clone(),
                script,
                self.caps,
                self.embedder,
                self.job_dir,
                handles,
                self.events,
                self.cancel,
                usage,
                self.started,
                self.index,
            )
            .await
            {
                Ok(v) => v,
                // A cancelled job stops the ladder dead; anything else is the
                // block failing, which is exactly what a ladder exists to
                // survive.
                Err(envelope) if envelope.status == JobStatus::Cancelled => {
                    return Err(LadderError::Fatal(Box::new(envelope)))
                }
                Err(envelope) => {
                    let reason = envelope
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "failed with no error detail".to_string());
                    return Err(LadderError::Exhausted {
                        reason,
                        escalated: false,
                        envelope: Some(Box::new(envelope)),
                    });
                }
            };

            let Some(field) = self.repeat_until else {
                break produced;
            };
            iterations += 1;
            if produced.get(field).and_then(|v| v.as_str()) == Some("done") {
                break produced;
            }
            let max = self
                .max_iterations
                .expect("repeat_until requires max_iterations, enforced at parse time");
            if iterations >= max {
                return Err(rejected(format!(
                    "did not reach repeat_until=\"done\" within max_iterations={max}"
                )));
            }
            current = produced;
        };

        if !self.expected.matches_value(&value) {
            return Err(rejected(format!(
                "produced {value}, which doesn't match the declared output `{}`",
                self.expected
            )));
        }

        if let Err(why) = self.checks.check_schemas(&value) {
            return Err(rejected(why));
        }

        match self
            .checks
            .run_judges(&input, &value, self.default_backend, self.alternates)
            .await
        {
            crate::accept::JudgeVerdict::Accepted => Ok(value),
            crate::accept::JudgeVerdict::Rejected(why) => Err(rejected(format!("judge: {why}"))),
            // A broken grader is worth retrying — the next attempt may get a
            // parseable verdict — but it is reported as what it is, so nobody
            // reads the escalation as "the model said no".
            crate::accept::JudgeVerdict::Unusable(why) => {
                Err(rejected(format!("judge gave no usable verdict: {why}")))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_stage(
    engine: &Engine,
    cache: &crate::module_cache::ModuleCache,
    backend: &Arc<dyn InferBackend>,
    module_bytes: &[u8],
    input: serde_json::Value,
    script: Option<&str>,
    caps: &Capabilities,
    embedder: Option<&Arc<dyn InferBackend>>,
    job_dir: &std::path::Path,
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
    // Extracted text, memoized per handle for this stage. Scoped exactly as
    // `doc_paths` is: a handle is job-scoped, so the text keyed by it can be
    // too, and both are dropped together when the stage ends.
    let mut doc_texts: std::collections::HashMap<u32, std::sync::Arc<String>> =
        std::collections::HashMap::new();
    // Page-tree counts, memoized the same way and for the same reason.
    let mut doc_pages: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();

    let mut guest = match Guest::new(engine, cache, module_bytes) {
        Ok(g) => g,
        Err(e) => {
            return Err(fail(
                error_codes::WASM_TRAP,
                blame(e.to_string()),
                usage.clone(),
            ))
        }
    };

    // A Script-kind stage's `module_bytes` is always the shared interpreter
    // (see `pipeline::resolve_and_load`), which expects its script text
    // wrapped alongside the real job input — the interpreter itself never
    // receives the raw input directly, and this is the one place per job
    // where that wrapping actually happens, since the script is fixed at
    // catalog time but the input is only known per job.
    let input = match script {
        Some(script) => serde_json::json!({
            "__cuttlefish_script": script,
            "input": input,
        }),
        None => input,
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
            Command::Embed { texts } => {
                let Some(backend) = embedder else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::UNSUPPORTED,
                        "this spec declares no `embedding_model`, so `embed` has nothing to \
                         call. Add one, e.g. `embedding_model = Ollama \"nomic-embed-text\";` \
                         — it is deliberately separate from `model`, since a chat model \
                         cannot produce embeddings."
                            .to_string(),
                        usage.clone(),
                    ));
                };
                match backend.embed(&texts).await {
                    Ok(vectors) => Event::Embedded { vectors },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                }
            }

            Command::Fetch { url } => {
                if !caps.allows_fetch(&url) {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    let granted = if caps.fetch_prefixes().is_empty() {
                        "this spec grants no `Fetch` capability at all".to_string()
                    } else {
                        format!("granted prefixes: {}", caps.fetch_prefixes().join(", "))
                    };
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        format!(
                            "fetch not permitted: {url}\nAdd `Fetch \"<prefix>\"` to the \
                             spec's `capabilities`. {granted}"
                        ),
                        usage.clone(),
                    ));
                }
                // Downloaded to the job's own directory and then opened like
                // any other file, so a fetched resource *is* a handle:
                // slice, identify, document_text and infer-with-images all
                // work on it with no further changes anywhere.
                match crate::fetch::fetch_to_file(&url, job_dir).await {
                    Ok(path) => match handles.open(&path) {
                        Ok((handle, len, kind)) => Event::Opened { handle, len, kind },
                        Err(e) => {
                            usage.duration_ms = started.elapsed().as_millis() as u64;
                            return Err(fail(
                                error_codes::UNSUPPORTED,
                                format!("opening the fetched copy of {url}: {e}"),
                                usage.clone(),
                            ));
                        }
                    },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                }
            }

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
                // Extract once per handle, not once per page. The old shape
                // re-extracted the whole document on every call, which made
                // a page walk quadratic: a 342-page filing meant 342 full
                // extractions, and across thousands of documents that is not
                // slow but unrunnable.
                let text = match doc_text(&mut doc_texts, handle, &path) {
                    Ok(t) => t,
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                };
                // The page-tree count is what makes the error honest: it can
                // say "1 addressable segment, 227 pages in the tree" rather
                // than blaming a scan.
                //
                // From `page_count`, never `inspect`: inspect also answers
                // has_text_layer, which it can only do by extracting the
                // whole document. Reaching for it here re-introduced the
                // very quadratic walk the text cache had just removed.
                let page_tree_count = doc_page_count(&mut doc_pages, handle, &path);
                match crate::documents::page_text_from(&text, page, page_tree_count) {
                    Ok(text) => Event::PageTexted { text },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(error_codes::UNSUPPORTED, e.to_string(), usage.clone()));
                    }
                }
            }

            Command::DocumentText { handle } => {
                let Some(path) = doc_paths.get(&handle).cloned() else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return Err(fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("no such handle: {handle}"),
                        usage.clone(),
                    ));
                };
                match doc_text(&mut doc_texts, handle, &path) {
                    // Cloned out of the Arc only here, at the point the text
                    // actually crosses to the guest.
                    Ok(text) => Event::PageTexted {
                        text: text.as_ref().clone(),
                    },
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

            Command::ImageOp { handle, op } => {
                // Pixels stay host-side: the guest names a handle, gets a
                // new handle, and never sees the bytes — same shape as
                // PageImage, so a transformed image is usable exactly
                // wherever a file-backed one is.
                let bytes = match handles.read_all(handle) {
                    Ok(b) => b,
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Err(fail(
                            error_codes::CAPABILITY_DENIED,
                            e.to_string(),
                            usage.clone(),
                        ));
                    }
                };
                match crate::images::apply(&bytes, &op) {
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

/// Extracted text for `handle`, extracting only on the first ask.
///
/// A `String` behind an `Arc` because a large PDF's text is megabytes and
/// every page of a walk would otherwise clone it.
fn doc_text(
    cache: &mut std::collections::HashMap<u32, std::sync::Arc<String>>,
    handle: u32,
    path: &std::path::Path,
) -> anyhow::Result<std::sync::Arc<String>> {
    if let Some(hit) = cache.get(&handle) {
        return Ok(hit.clone());
    }
    let text = std::sync::Arc::new(crate::documents::document_text(path)?);
    cache.insert(handle, text.clone());
    Ok(text)
}

/// The page-tree count for `handle`, read once.
///
/// Zero when the document cannot be loaded: this value exists only to make
/// an error message more informative, so failing to obtain it must never
/// turn into a second failure.
fn doc_page_count(
    cache: &mut std::collections::HashMap<u32, u32>,
    handle: u32,
    path: &std::path::Path,
) -> u32 {
    if let Some(hit) = cache.get(&handle) {
        return *hit;
    }
    let count = crate::documents::page_count(path).unwrap_or(0);
    cache.insert(handle, count);
    count
}
