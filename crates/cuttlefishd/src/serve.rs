//! Serving the API over a machine-local, OS-permission-gated transport: a
//! unix domain socket on unix, a named pipe on Windows.
//!
//! Both are deliberate choices over a TCP listener. The daemon's access
//! control *is* the endpoint's own permissions — it executes wasm and drives
//! local models, so "any process on this machine may connect" is a materially
//! weaker posture than "whoever can open this socket file may connect". A
//! loopback TCP port has no such gate and would need an auth token bolted on
//! just to get back to where we already are. Named pipes are the Windows
//! analogue of the unix socket, not a workaround for the lack of one.

use axum::Router;
use std::path::Path;

/// Serve `app` on `endpoint` until `shutdown` resolves.
///
/// `endpoint` is a socket path on unix and a pipe name (`\\.\pipe\...`) on
/// Windows; see [`cuttlefish_core::endpoint::default_endpoint`]. `shutdown`
/// is the portable stand-in for a `SIGTERM` handler — Windows has no signal
/// this daemon could install one for, so graceful stop is instead triggered
/// by the `POST /shutdown` route resolving this future; see `api::AppState`.
pub async fn serve(
    app: Router,
    endpoint: &Path,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        serve_unix(app, endpoint, shutdown).await
    }
    #[cfg(windows)]
    {
        serve_named_pipe(app, endpoint, shutdown).await
    }
}

/// Serve `app` on a unix socket at `sock_path` until `shutdown` resolves.
///
/// axum 0.8's `serve` accepts any listener, so this is a handful of lines. On
/// 0.7 it took a concrete `TcpListener` and a unix socket needed a hand-rolled
/// hyper accept loop over `hyper-util` and `tower`; if you find yourself
/// reintroducing that, check the axum version first.
#[cfg(unix)]
pub async fn serve_unix(
    app: Router,
    sock_path: &Path,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    use tokio::net::UnixListener;

    // A stale socket file from a previous run makes bind() fail with EADDRINUSE
    // even though nothing is listening on it.
    let _ = std::fs::remove_file(sock_path);

    let listener = UnixListener::bind(sock_path)?;
    eprintln!("cuttlefishd listening on {}", sock_path.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Serve `app` on a Windows named pipe until `shutdown` resolves.
#[cfg(windows)]
pub async fn serve_named_pipe(
    app: Router,
    pipe_name: &Path,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = windows_pipe::NamedPipeListener::bind(pipe_name)?;
    eprintln!("cuttlefishd listening on {}", pipe_name.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// A named pipe presented as an [`axum::serve::Listener`].
///
/// Unlike a socket, a named pipe server does not multiplex: one server
/// *instance* serves exactly one client. To stay continuously available you
/// must have the next instance created before handing the current one off, or
/// a client connecting in that window is refused. This module exists to own
/// that rotation so nothing above it has to know about it.
#[cfg(windows)]
mod windows_pipe {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    pub struct NamedPipeListener {
        name: OsString,
        /// The instance currently waiting for a client. Always present:
        /// `accept` creates its replacement before yielding the connected one.
        pending: NamedPipeServer,
    }

    impl NamedPipeListener {
        pub fn bind(name: &Path) -> io::Result<Self> {
            let name = name.as_os_str().to_os_string();
            // `first_pipe_instance` makes a second daemon on the same name fail
            // loudly here rather than silently taking half the connections.
            let pending = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)?;
            Ok(Self { name, pending })
        }
    }

    impl axum::serve::Listener for NamedPipeListener {
        type Io = NamedPipeServer;
        /// A pipe connection has no peer address to report. axum only requires
        /// this to be `Send`.
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                // Waits until a client opens the pipe.
                if let Err(e) = self.pending.connect().await {
                    eprintln!("cuttlefishd: named pipe connect failed: {e}");
                    continue;
                }

                // Create the *next* instance before yielding this one, so the
                // pipe name is never momentarily unavailable.
                match ServerOptions::new().create(&self.name) {
                    Ok(next) => {
                        let connected = std::mem::replace(&mut self.pending, next);
                        return (connected, ());
                    }
                    Err(e) => {
                        // Yielding this connection would leave no listener
                        // behind, so drop it and keep serving instead. The
                        // trait requires accept to handle its own errors.
                        eprintln!("cuttlefishd: could not create the next pipe instance: {e}");
                        self.pending.disconnect().ok();
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(())
        }
    }
}
