//! Renders one PDF page and exits.
//!
//! Exists so that a pdfium crash is a dead child process rather than a dead
//! daemon. The daemon normally re-execs itself as the worker; this standalone
//! binary is what tests point at, since a test binary cannot re-exec itself into
//! a worker without libtest trying to parse the worker's arguments as test
//! filters.

fn main() {
    cuttlefish_host::render_worker::run_if_worker();
    eprintln!(
        "this binary only runs as a render worker; it expects {}",
        cuttlefish_host::render_worker::WORKER_ARG
    );
    std::process::exit(2);
}
