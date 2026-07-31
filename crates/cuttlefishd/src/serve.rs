//! Serving the API over a unix domain socket.

use axum::Router;
use std::path::Path;
use tokio::net::UnixListener;

/// Serve `app` on a unix socket at `sock_path` until the process ends.
///
/// axum 0.8's `serve` accepts any listener, so this is a handful of lines. On
/// 0.7 it took a concrete `TcpListener` and a unix socket needed a hand-rolled
/// hyper accept loop over `hyper-util` and `tower`; if you find yourself
/// reintroducing that, check the axum version first.
pub async fn serve_unix(app: Router, sock_path: &Path) -> anyhow::Result<()> {
    // A stale socket file from a previous run makes bind() fail with EADDRINUSE
    // even though nothing is listening on it.
    let _ = std::fs::remove_file(sock_path);

    let listener = UnixListener::bind(sock_path)?;
    eprintln!("cuttlefishd listening on {}", sock_path.display());
    axum::serve(listener, app).await?;
    Ok(())
}
