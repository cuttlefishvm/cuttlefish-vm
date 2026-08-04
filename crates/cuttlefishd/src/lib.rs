//! The cuttlefish daemon: accepts delegated jobs, runs them, streams results.
//!
//! The transport decision is recorded here, because it is the choice most
//! likely to be revisited by someone who was not present for it.
//!
//! # Transport: HTTP over a unix domain socket
//!
//! Clients talk to the daemon over HTTP on a unix domain socket, with
//! server-sent events for live job output.
//!
//! It is worth being precise about what that does and does not mean, because
//! "why HTTP instead of IPC?" is the obvious question and it contains a false
//! dichotomy. A unix domain socket *is* inter-process communication. The socket
//! was never in question; HTTP is only the framing carried over it. The real
//! choice is which protocol rides the socket.
//!
//! HTTP was chosen for reasons that all reduce to *nobody has to write protocol
//! code*:
//!
//! - Every language has a mature client, so consuming cuttlefish costs no
//!   bespoke implementation. The CLI in this workspace is a thin wrapper around
//!   an off-the-shelf HTTP client.
//! - It is debuggable by hand. `curl --unix-socket` reproduces any client
//!   interaction, which matters enormously while the reactor loop and the
//!   capability model are still being proven.
//! - Server-sent events supply streaming *and* `Last-Event-ID` replay. The
//!   replay half is not incidental: a client that submits a job and then
//!   connects to the event stream would otherwise miss everything a fast job
//!   emitted in between.
//! - Exposing the daemon over TCP for a remote caller becomes a config change
//!   rather than a second protocol.
//!
//! ## The alternative, and the one argument that genuinely favours it
//!
//! A leaner framing — length-prefixed JSON, JSON-RPC, gRPC — would cost less per
//! message and would be bidirectional, letting cancellation travel in-band
//! instead of as a separate `DELETE`. Neither matters much here: this transport
//! carries jobs, not packets, and cancellation is rare.
//!
//! JSON-RPC deserves a specific mention as the strongest of those: it is what
//! MCP and LSP speak, so it is the framing that agent tooling already
//! understands, and adopting it would leave cuttlefish a short step from being
//! usable directly as an MCP server.
//!
//! The genuinely compelling argument for a raw socket protocol, though, is one
//! HTTP cannot answer at all: **file descriptor passing**. A unix socket can
//! transfer an open file descriptor via `SCM_RIGHTS`. For this project that is
//! not a micro-optimisation, it changes the security model:
//!
//! - Today a caller passes a *path*, the daemon checks it against the job's
//!   granted capabilities, and then opens it. Check and open are separate
//!   syscalls, so there is a window in which the path can be swapped —
//!   precisely the race that symlink attacks exploit.
//! - If the caller passed an already-open descriptor instead, there would be no
//!   window, because the descriptor *is* the capability. The daemon would need
//!   no filesystem access to the path at all, which is a materially stronger
//!   version of the privacy guarantee this project exists to provide.
//!
//! A related, smaller benefit: `SO_PEERCRED` lets the daemon establish the uid
//! and pid of its peer, which matters on a shared machine.
//!
//! Neither of these is a reason to replace the control plane. Descriptor
//! passing cannot ride HTTP no matter what else is decided, so it needs a side
//! channel in *every* design — which makes it additive later rather than a
//! constraint now. The path-based capability check keeps its own defence in the
//! meantime: it canonicalises both sides before comparing, which closes the
//! traversal and symlink cases it can close without descriptor passing.
//!
//! Revisit this decision if descriptor passing becomes required, or if
//! cuttlefish should speak MCP natively. Do not revisit it for performance
//! without a measurement showing the transport is the bottleneck; at job
//! granularity it will not be.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod state;

// Unix domain sockets only exist on unix, and tokio exposes no `UnixListener`
// elsewhere. The rest of this crate — job store, router, handlers — is portable
// and still compiles and tests on Windows; only the transport is gated, so a
// Windows build fails loudly at `serve` rather than silently lacking it.
#[cfg(unix)]
pub mod serve;

/// Scan every job directory under `jobs_root` for a ledger whose
/// `job_status` is still `Running` — i.e. a job that was mid-flight when the
/// daemon last stopped — and record it in `jobs` as [`cuttlefish_abi::JobStatus::Interrupted`].
///
/// Called once at startup, and directly by tests that build a `JobStore`
/// without going through `main()`. Nothing here resumes a job automatically:
/// see the "Resume is a signal, not an automatic action" section of the
/// design doc. `current_fingerprint` is passed through to `Ledger::open`,
/// but is only meaningful for a ledger that doesn't exist yet — every ledger
/// this loop opens already exists (its presence is what selected the
/// directory), so the value recorded at that ledger's original creation is
/// what's read back, untouched.
///
/// A single job directory whose ledger can't be opened or read (e.g.
/// corrupted by a hard crash mid-write) is logged and skipped, not treated
/// as fatal — the whole point of this scan is to surface crash damage, so it
/// must not itself go down because of the very class of crash it looks for.
/// Only a failure to list `jobs_root` at all is fatal, since that means the
/// scan cannot proceed in any capacity.
pub async fn scan_for_interrupted_jobs(
    jobs_root: &std::path::Path,
    current_fingerprint: &str,
    jobs: &state::JobStore,
) -> anyhow::Result<()> {
    if !jobs_root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(jobs_root)? {
        let entry = entry?;
        let ledger_path = entry.path().join("ledger.sqlite");
        if !ledger_path.exists() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let ledger = match cuttlefish_host::ledger::Ledger::open(&ledger_path, current_fingerprint)
        {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "warning: could not open ledger for job `{id}` at startup scan, skipping: {e}"
                );
                continue;
            }
        };
        match ledger.job_status() {
            Ok(cuttlefish_host::ledger::LedgerJobStatus::Running) => {
                jobs.mark_interrupted(id).await;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("warning: could not read job status for job `{id}` at startup scan, skipping: {e}");
            }
        }
    }
    Ok(())
}
