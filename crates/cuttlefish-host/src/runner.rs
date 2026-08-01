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
    /// The compiled guest module.
    pub module_bytes: Vec<u8>,
    /// The job's input, handed to the guest's `init`.
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
    let mut handles = Handles::default();
    // Documents are read from their path rather than their descriptor — both
    // extraction and rendering want a file. Kept beside the handle table so the
    // two are dropped together at the end of the job.
    let mut doc_paths: std::collections::HashMap<u32, std::path::PathBuf> =
        std::collections::HashMap::new();

    let mut guest = match Guest::new(&engine, &job.module_bytes) {
        Ok(g) => g,
        Err(e) => return fail(error_codes::WASM_TRAP, e.to_string(), usage),
    };

    let mut command = match guest.call_init(&job.input) {
        Ok(c) => c,
        Err(e) => return fail(error_codes::WASM_TRAP, e.to_string(), usage),
    };

    loop {
        if cancel.is_cancelled() {
            usage.duration_ms = started.elapsed().as_millis() as u64;
            return cancelled(usage, "job cancelled");
        }

        let event = match command {
            Command::Done { result } => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return Envelope {
                    status: JobStatus::Completed,
                    result: Some(result),
                    error: None,
                    usage,
                };
            }
            Command::Fail { code, message } => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return fail(&code, message, usage);
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
                if !job.caps.allows_read(&p) {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("read not permitted: {path}"),
                        usage,
                    );
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
                        return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
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
                    return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
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
                    return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
                }
            },

            Command::PageText { handle, page } => {
                let Some(path) = doc_paths.get(&handle).cloned() else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("no such handle: {handle}"),
                        usage,
                    );
                };
                match crate::documents::page_text(&path, page) {
                    Ok(text) => Event::PageTexted { text },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(error_codes::UNSUPPORTED, e.to_string(), usage);
                    }
                }
            }

            Command::PageImage { handle, page } => {
                let Some(path) = doc_paths.get(&handle).cloned() else {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("no such handle: {handle}"),
                        usage,
                    );
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
                        return fail(error_codes::UNSUPPORTED, e.to_string(), usage);
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
                    return fail(
                        error_codes::UNSUPPORTED,
                        format!(
                            "this job supplied {} image(s), but the backend serving `{}` cannot \
                             accept them. Use a vision-capable model through the `ollama` \
                             provider, or change the block to send text only.",
                            images.len(),
                            backend.model_name()
                        ),
                        usage,
                    );
                }
                let mut image_bytes = Vec::with_capacity(images.len());
                for handle in &images {
                    match handles.read_all(*handle) {
                        Ok(bytes) => image_bytes.push(bytes),
                        Err(e) => {
                            usage.duration_ms = started.elapsed().as_millis() as u64;
                            return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
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
                    return fail(error_codes::WASM_TRAP, message, usage);
                }

                // Tokens generated in the same poll as the last one are still
                // queued; forward them so the stream is complete.
                while let Ok(tok) = rx.try_recv() {
                    let _ = events.send(JobEvent::Token(tok)).await;
                }

                match outcome {
                    None => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return cancelled(usage, "cancelled during inference");
                    }
                    Some(Err(e)) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(error_codes::MODEL_LOAD_FAILED, e.to_string(), usage);
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
                return fail(error_codes::WASM_TRAP, e.to_string(), usage);
            }
        };
    }
}
