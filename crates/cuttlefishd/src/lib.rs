//! The cuttlefish daemon: accepts delegated jobs, runs them, streams results.
//!
//! This crate is a stub. The job store, scheduler, and HTTP surface land next;
//! what is recorded here is the transport decision, because that is the choice
//! most likely to be revisited by someone who was not present for it.
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
