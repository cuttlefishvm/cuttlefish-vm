//! Rendering PDF pages in a subprocess, so a crash cannot take the daemon down.
//!
//! # Why this exists
//!
//! pdfium is a large C++ library, and it segfaults on input that other parsers
//! accept — this project has a PDF that `lopdf` reads without complaint and
//! pdfium dies on. That is not a bug to be fixed here; it is what handing
//! untrusted bytes to a C++ parser is like.
//!
//! In-process, a segfault kills the daemon. Not the job — the *daemon*, and with
//! it every other job running alongside, plus their results. The whole system is
//! otherwise built so that a failing job fails alone: capabilities are checked
//! per job, contexts are per job, a wasm trap ends one job. A renderer that can
//! take down the process is the one thing that breaks that property.
//!
//! So rendering happens in a child process. A crash there becomes a signal on a
//! wait status, which is a normal error the offending job reports and everything
//! else survives.
//!
//! # How the child is chosen
//!
//! Normally the child is *this same executable*, re-invoked with a hidden
//! argument, which guarantees it is exactly the build the parent is running — a
//! separately shipped binary can drift out of sync in ways that appear only at
//! runtime.
//!
//! That does not work for tests: a libtest binary cannot re-exec itself, because
//! libtest would read the worker's arguments as test filters. So
//! [`WORKER_EXE_ENV`] can name an executable instead, and the crate ships a
//! `cuttlefish-render-worker` binary for exactly that purpose.
//!
//! The cost is one process spawn per rendered page. Against the render itself
//! and the vision-model inference that follows it, that is not measurable.

// `Write` is only reached by the rendering path, which is feature-gated —
// importing it unconditionally warns in a build without the feature.
use std::io::Read;
#[cfg(feature = "pdf-render")]
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// The argument that turns this executable into a render worker.
///
/// Deliberately unlikely to collide with a real argument, and checked before any
/// other parsing so a worker never runs the daemon's own startup.
pub const WORKER_ARG: &str = "--__cuttlefish_render_worker";

/// If this process was spawned as a render worker, do that work and exit.
///
/// Call first thing in `main`. Returns only when this is a normal invocation.
///
/// The protocol is deliberately minimal: page number and width on the command
/// line, the PDF path too, PNG bytes on stdout, a message on stderr, and the
/// exit code carrying success. Anything richer would need a serialization format
/// on both sides of a boundary whose entire purpose is that one side may die
/// unexpectedly.
pub fn run_if_worker() {
    let args: Vec<String> = std::env::args().collect();
    let Some(position) = args.iter().position(|a| a == WORKER_ARG) else {
        return;
    };

    // Recognised, but this build cannot render. Say exactly that. The
    // alternative — returning and letting the caller parse these arguments
    // as its own — produced `Error: usage: cuttlefishd <spec> ...` as a
    // per-item failure, which names neither rendering nor the feature and
    // sends the reader looking at their spec.
    #[cfg(not(feature = "pdf-render"))]
    {
        let _ = position;
        eprintln!(
            "this binary was spawned as a PDF render worker but was built without the \
             `pdf-render` feature. The daemon binary does the rendering, so it needs the \
             feature too: `cargo build -p cuttlefishd --features pdf-render`. Enabling it \
             only on cuttlefish-host is not enough."
        );
        std::process::exit(2);
    }

    #[cfg(feature = "pdf-render")]
    {
        let fail = |message: &str| -> ! {
            eprintln!("{message}");
            std::process::exit(2);
        };

        let (Some(path), Some(page), Some(width)) = (
            args.get(position + 1),
            args.get(position + 2).and_then(|s| s.parse::<u32>().ok()),
            args.get(position + 3).and_then(|s| s.parse::<u16>().ok()),
        ) else {
            fail("render worker: expected <path> <page> <width>");
        };

        match crate::documents::render_page_in_process(Path::new(path), page, width) {
            Ok(png) => {
                if let Err(e) = std::io::stdout().write_all(&png) {
                    fail(&format!("render worker: writing output: {e}"));
                }
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            Err(e) => fail(&format!("{e}")),
        }
    }
}

/// Names an executable to use as the render worker instead of re-execing this
/// one.
///
/// Exists for tests: a libtest binary cannot re-exec itself as a worker, because
/// libtest would try to read the worker's arguments as test filters.
pub const WORKER_EXE_ENV: &str = "CUTTLEFISH_RENDER_WORKER";

/// Render a page in a child process, returning PNG bytes.
pub fn render_page(path: &Path, page: u32, width: u16) -> anyhow::Result<Vec<u8>> {
    let exe = match std::env::var(WORKER_EXE_ENV) {
        Ok(exe) if !exe.is_empty() => std::path::PathBuf::from(exe),
        _ => std::env::current_exe().map_err(|e| {
            anyhow::anyhow!("locating this executable to spawn a render worker: {e}")
        })?,
    };

    let mut child = Command::new(exe)
        .arg(WORKER_ARG)
        .arg(path)
        .arg(page.to_string())
        .arg(width.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning the render worker: {e}"))?;

    let mut png = Vec::new();
    let mut message = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut png)?;
    }
    if let Some(mut err) = child.stderr.take() {
        err.read_to_string(&mut message)?;
    }
    let status = child.wait()?;

    if status.success() {
        return Ok(png);
    }

    // A signal means the renderer crashed rather than refused. Saying so
    // distinguishes "this PDF is malformed in a way pdfium survives" from "this
    // PDF killed the renderer", and the second is worth knowing about.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            anyhow::bail!(
                "the PDF renderer crashed (signal {signal}) on {}. The page was \
                 not rendered, but the daemon is unaffected — this is why \
                 rendering runs in a separate process.",
                path.display()
            );
        }
    }

    let message = message.trim();
    if message.is_empty() {
        anyhow::bail!(
            "the PDF renderer failed with status {status} on {}",
            path.display()
        );
    }
    anyhow::bail!("{message}");
}
