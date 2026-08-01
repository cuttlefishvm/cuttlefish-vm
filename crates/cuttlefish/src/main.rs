//! The `cuttlefish` command-line client.
//!
//! A thin wrapper over the daemon's HTTP API — deliberately thin, because the
//! API is the real interface and an agent will call it directly. This exists so
//! a human can drive the same thing without writing a client, and so that
//! anything awkward to do by hand shows up as awkward here too.
//!
//! Exit status is the machine-readable result: `0` completed, `1` failed, `2`
//! cancelled. A shell script or an agent can branch on that without parsing
//! stdout, which stays pure JSON for the same reason.
//!
//! Unix-only for now, for the same reason as the daemon: it speaks to a unix
//! domain socket, and `reqwest`'s `unix_socket` is `cfg(unix)`. Everything
//! unix-specific lives in one gated module rather than being scattered through
//! per-item `cfg` attributes, so a Windows build has no unused imports or dead
//! types to warn about.

#[cfg(unix)]
mod cli {
    use anyhow::{bail, Context};
    use clap::{Parser, Subcommand};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// How long to wait between status polls.
    ///
    /// Short enough to feel immediate, long enough not to spin. The daemon also
    /// offers a streaming endpoint; this polls because a result is retained, so
    /// watching the stream is not required in order to observe one.
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    #[derive(Parser)]
    #[command(
        name = "cuttlefish",
        version,
        about = "Client for the cuttlefish daemon"
    )]
    pub struct Cli {
        #[command(subcommand)]
        command: Cmd,
    }

    #[derive(Subcommand)]
    enum Cmd {
        /// Submit a job and wait for its result.
        Run {
            /// Path to the daemon's unix socket.
            #[arg(long, default_value = "/tmp/cuttlefish.sock")]
            socket: PathBuf,
            /// Which spec to run.
            #[arg(long)]
            spec: String,
            /// Job input, as a JSON object.
            #[arg(long)]
            input: String,
        },
        /// List what the daemon can run.
        Specs {
            /// Path to the daemon's unix socket.
            #[arg(long, default_value = "/tmp/cuttlefish.sock")]
            socket: PathBuf,
        },
    }

    /// Parse arguments and carry out the requested command.
    pub async fn main() -> anyhow::Result<()> {
        match Cli::parse().command {
            Cmd::Specs { socket } => specs(&socket).await,
            Cmd::Run {
                socket,
                spec,
                input,
            } => run(&socket, &spec, &input).await,
        }
    }

    /// Build a client bound to a unix socket.
    ///
    /// `as_path()`, not `&socket`: reqwest's sealed `UnixSocketProvider` covers
    /// `&Path` and `PathBuf` but not `&PathBuf`, and no deref coercion happens
    /// at an `impl Trait` parameter.
    fn client(socket: &Path) -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .context("building the unix-socket client")
    }

    async fn specs(socket: &Path) -> anyhow::Result<()> {
        // The authority in these URLs is ignored — the socket decides where the
        // request goes — but reqwest still requires a syntactically valid URL.
        let body: serde_json::Value = client(socket)?
            .get("http://localhost/specs")
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?
            .json()
            .await?;

        println!("{}", serde_json::to_string_pretty(&body)?);
        Ok(())
    }

    async fn run(socket: &Path, spec: &str, input: &str) -> anyhow::Result<()> {
        let input: serde_json::Value =
            serde_json::from_str(input).context("--input must be JSON")?;
        let client = client(socket)?;

        let submitted = client
            .post("http://localhost/jobs")
            .json(&serde_json::json!({ "spec": spec, "input": input }))
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;

        if !submitted.status().is_success() {
            let status = submitted.status();
            bail!(
                "daemon rejected the job: {status} {}",
                submitted.text().await?
            );
        }

        let job_id = submitted.json::<serde_json::Value>().await?["job_id"]
            .as_str()
            .context("daemon response had no job_id")?
            .to_string();

        loop {
            let body: serde_json::Value = client
                .get(format!("http://localhost/jobs/{job_id}"))
                .send()
                .await?
                .json()
                .await?;

            let status = body["status"].as_str().unwrap_or("running");
            match status {
                "completed" | "failed" | "cancelled" => {
                    // stdout stays pure JSON so it can be piped into a parser;
                    // the outcome travels in the exit status instead.
                    println!("{}", serde_json::to_string_pretty(&body["envelope"])?);
                    std::process::exit(match status {
                        "completed" => 0,
                        "failed" => 1,
                        _ => 2,
                    });
                }
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::main().await
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "cuttlefish talks to the daemon over a unix domain socket and does not \
         run on this platform yet. The library crates are cross-platform; only \
         the transport is unix-only."
    );
    std::process::exit(1);
}
