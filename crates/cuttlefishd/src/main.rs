//! Entry point for the cuttlefish daemon.
//!
//! See the library docs for the transport design and job lifecycle.

use anyhow::Context;
use cuttlefish_host::infer::StubBackend;
use cuttlefishd::{api, serve, state};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let usage = "usage: cuttlefishd <spec> <block.wasm> [socket]";
    let mut args = std::env::args().skip(1);
    let spec_path = PathBuf::from(args.next().context(usage)?);
    let wasm_path = args.next().context(usage)?;
    let sock_path = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/cuttlefish.sock".into()));

    let mut spec = cuttlefish_core::spec::parse_spec(
        &std::fs::read_to_string(&spec_path)
            .with_context(|| format!("reading {}", spec_path.display()))?,
    )?;

    // Capability roots are written relative to the spec file, not to wherever
    // the daemon happens to be started from. Resolving them here is what keeps
    // `Read "./docs"` meaning the same thing regardless of the working
    // directory — otherwise the grant silently changes with `cd`.
    let spec_dir = spec_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    spec.read_roots = spec.read_roots.iter().map(|r| spec_dir.join(r)).collect();

    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(StubBackend::default()),
        jobs: state::JobStore::default(),
        spec: Arc::new(spec),
        module_bytes: Arc::new(
            std::fs::read(&wasm_path).with_context(|| format!("reading {wasm_path}"))?,
        ),
    };

    serve::serve_unix(api::router(state), &sock_path).await
}
