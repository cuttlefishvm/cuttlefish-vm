//! Entry point for the cuttlefish daemon.
//!
//! See the library docs for the transport design and job lifecycle.
//!
//! The daemon serves over a unix domain socket, so it runs on unix only for now.
//! The rest of the workspace is cross-platform; a Windows build produces a
//! binary that explains itself and exits, rather than one that silently does
//! nothing. Adding a TCP listener would lift this — see the transport notes in
//! the library docs.

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use cuttlefish_host::backend::Registry;
    use cuttlefishd::{api, serve, state};
    use std::path::PathBuf;
    use std::sync::Arc;

    // First, before argument parsing or anything else: this process may be a
    // render worker that the daemon spawned, in which case it renders one page
    // and exits. Doing this later would have a worker try to start a daemon.
    #[cfg(feature = "pdf-render")]
    cuttlefish_host::render_worker::run_if_worker();

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

    // The spec names a provider; the registry decides what serves it. Adding a
    // backend therefore changes neither this file nor the spec parser.
    let backend = Registry::with_builtins()
        .resolve(&spec.model)
        .with_context(|| format!("resolving model `{}`", spec.model))?;
    eprintln!("cuttlefishd serving `{}` via {}", spec.name, spec.model);

    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend,
        jobs: state::JobStore::default(),
        spec: Arc::new(spec),
        module_bytes: Arc::new(
            std::fs::read(&wasm_path).with_context(|| format!("reading {wasm_path}"))?,
        ),
    };

    serve::serve_unix(api::router(state), &sock_path).await
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "cuttlefishd serves over a unix domain socket and does not run on this \
         platform yet. The library crates are cross-platform; only the transport \
         is unix-only."
    );
    std::process::exit(1);
}
