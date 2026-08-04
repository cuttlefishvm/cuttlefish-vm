//! Entry point for the cuttlefish daemon.
//!
//! See the library docs for the transport design and job lifecycle.
//!
//! The daemon serves over a machine-local, OS-permission-gated transport: a
//! unix domain socket on unix, a named pipe on Windows. See `serve` for why
//! that pairing rather than a TCP listener.

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

    let usage = "usage: cuttlefishd <spec> [--wasm <path>] [endpoint]";
    let mut args = std::env::args().skip(1);
    let spec_path = PathBuf::from(args.next().context(usage)?);

    // `--wasm` lives behind a flag rather than a second required positional
    // so that a later, optional `endpoint` positional never has to share a
    // slot with it — two adjacent optional positionals would be structurally
    // ambiguous (which one did the caller mean?), whereas a flag can never be
    // confused with a bare value no matter what that value looks like.
    let mut wasm_path: Option<String> = None;
    let mut endpoint_arg: Option<String> = None;
    while let Some(arg) = args.next() {
        if arg == "--wasm" {
            wasm_path = Some(args.next().context("--wasm requires a path")?);
        } else if endpoint_arg.is_none() {
            endpoint_arg = Some(arg);
        } else {
            anyhow::bail!(usage);
        }
    }
    let endpoint = endpoint_arg
        .map(PathBuf::from)
        .unwrap_or_else(cuttlefish_core::endpoint::default_endpoint);

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

    // Typecheck before serving. A seam mismatch is a property of the spec, not
    // of any one job, so it should stop startup rather than fail every job
    // identically once traffic arrives.
    let engine = Arc::new(wasmtime::Engine::default());
    // A home directory must be discoverable even for a pipeline made entirely
    // of direct `.wasm` paths that will never touch the catalog — resolving
    // it lazily, only for specs that turn out to need a catalog lookup, would
    // save that requirement for a narrow case (no `$HOME`, no
    // `CUTTLEFISH_HOME`) at the cost of a second, independent copy of
    // `resolve_and_load`'s own Direct-vs-Cataloged decision here. Simpler and
    // less fragile to require it upfront and let this fail loudly and early.
    let catalog_root = cuttlefish_host::catalog::default_root()
        .context("could not determine home directory; set CUTTLEFISH_HOME")?;
    let catalog = cuttlefish_host::catalog::Catalog::open(catalog_root);
    let resolved: std::collections::HashMap<String, cuttlefish_host::pipeline::ResolvedInput> =
        spec.nodes
            .nodes
            .iter()
            .map(|(name, node)| {
                cuttlefish_host::pipeline::resolve_and_load(
                    &catalog,
                    spec_dir,
                    &node.block.to_string_lossy(),
                    cuttlefish_host::catalog::ResolutionContext::Interactive,
                )
                .map(|input| (name.clone(), input))
            })
            .collect::<Result<_, _>>()
            .with_context(|| format!("resolving the pipeline for `{}`", spec.name))?;
    let checked =
        cuttlefish_host::dag::check_graph(&engine, &spec.nodes, &spec.branches, &resolved)
            .with_context(|| format!("checking the pipeline for `{}`", spec.name))?;

    // A stage can typecheck cleanly as a Bundle (its cached signature parses
    // and its seams fit) while still being unrunnable: `stages` below feeds
    // every stage's raw bytes straight to the wasm engine, which cannot
    // instantiate a `.cfbundle` container at all. Nested-subjobs (actually
    // executing a bundle as a sub-job) isn't built yet, so reject this at
    // startup — once, with a clear message — rather than let every job
    // against this stage fail with a confusing wasm trap. `cuttlefish build`
    // is unaffected: embedding a bundle stage in an outer bundle is fine, it
    // is only *running* one directly that isn't supported yet.
    if let Some(bundle_stage) = checked
        .nodes
        .iter()
        .find(|n| n.kind == cuttlefish_host::catalog::ArtifactKind::Bundle)
    {
        anyhow::bail!(
            "stage `{}` resolved to a bundle, not a block. Running a nested bundle as a \
             sub-job isn't implemented yet — only single-block pipelines can be served today. \
             `cuttlefish build` can still embed it, it just can't run here yet.",
            bundle_stage.name
        );
    }

    eprintln!(
        "cuttlefishd pipeline `{}`: {} node(s)",
        spec.name,
        checked.nodes.len()
    );

    // Detect jobs that were still running when this daemon last stopped,
    // before `checked.nodes` gets moved into `AppState` below — the
    // fingerprint is computed from a reference to it.
    let jobs = state::JobStore::default();
    let jobs_root = cuttlefish_host::ledger::jobs_root()
        .context("could not determine home directory; set CUTTLEFISH_HOME")?;
    let current_fingerprint = cuttlefish_host::dag::graph_fingerprint(&checked.nodes);
    cuttlefishd::scan_for_interrupted_jobs(&jobs_root, &current_fingerprint, &jobs).await?;

    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());

    let state = api::AppState {
        engine,
        backend,
        jobs,
        spec: Arc::new(spec),
        checked_nodes: Arc::new({
            let mut nodes = checked.nodes;
            // `--wasm` overrides the FIRST node in topological order — same
            // convention as when this was a required positional — so an
            // operator can still point at a freshly built block without
            // editing the spec. A path that fails to read is reported rather
            // than silently ignored: swallowing it would mean a typo'd
            // `--wasm` path quietly serves the spec's original node with no
            // indication the flag was ever seen, which is exactly the kind of
            // silent misconfiguration this codebase's other startup checks
            // (the bundle-stage check above, the home-directory checks) treat
            // as loud, early failures rather than something to paper over.
            if let Some(wasm_path) = &wasm_path {
                let bytes = std::fs::read(wasm_path)
                    .with_context(|| format!("reading --wasm path {wasm_path}"))?;
                if let Some(first) = nodes.first_mut() {
                    first.module_bytes = bytes;
                }
            }
            nodes
        }),
        exclusive_to: Arc::new(checked.exclusive_to),
        shutdown: shutdown.clone(),
    };

    let shutdown_signal = {
        let shutdown = shutdown.clone();
        async move { shutdown.notified().await }
    };
    serve::serve(api::router(state), &endpoint, shutdown_signal).await
}
