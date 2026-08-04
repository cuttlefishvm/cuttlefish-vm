//! The `cuttlefish` command-line client.
//!
//! A thin wrapper over the daemon's HTTP API — deliberately thin, because the
//! API is the real interface and an agent will call it directly. This exists so
//! a human can drive the same thing without writing a client, and so that
//! anything awkward to do by hand shows up as awkward here too.
//!
//! `catalog` and `build` are the exceptions: both are purely local
//! filesystem operations (see `cuttlefish_host::catalog` and
//! `cuttlefish_host::bundle`) with no daemon involved at all, since the
//! block catalog and the pipeline linker are both designed to work
//! standalone.
//!
//! Exit status is the machine-readable result: `0` completed, `1` failed, `2`
//! cancelled. A shell script or an agent can branch on that without parsing
//! stdout, which stays pure JSON for the same reason.
//!
//! `cli` holds the argument parsing and the commands that touch nothing but
//! the filesystem (`catalog`, `build`); `daemon` holds the ones that talk to
//! the daemon (`run`, `specs`). The split is by dependency, and it is no
//! longer a platform boundary: both halves compile everywhere now that the
//! transport has a named-pipe implementation on Windows.
//!
//! Argument parsing is deliberately not split: one `clap` derive covers every
//! subcommand on every platform.

mod cli {
    use anyhow::{bail, Context};
    use clap::{Parser, Subcommand};
    use std::path::{Path, PathBuf};

    /// The daemon's default endpoint, in the client's own words. Deliberately
    /// delegates to the daemon crate rather than restating the path, so the
    /// two can never drift apart.
    fn cuttlefishd_endpoint() -> PathBuf {
        cuttlefish_core::endpoint::default_endpoint()
    }

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
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
            /// Which spec to run.
            #[arg(long)]
            spec: String,
            /// Job input, as a JSON object.
            #[arg(long)]
            input: String,
        },
        /// List what the daemon can run.
        Specs {
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
        },
        /// Manage the local block catalog (~/.cuttlefish/catalog by default).
        /// Purely local filesystem operations — no running daemon required.
        Catalog {
            #[command(subcommand)]
            action: CatalogCmd,
        },
        /// Link and verify a spec's pipeline into a distributable .cfbundle.
        /// Purely local — no running daemon required.
        Build {
            /// The .cuttlefish spec to build.
            spec: PathBuf,
            /// Where to write the bundle. Defaults to the spec's own path
            /// with its extension set to .cfbundle.
            #[arg(short, long)]
            output: Option<PathBuf>,
        },
    }

    /// `cuttlefish catalog` subcommands.
    #[derive(Subcommand)]
    enum CatalogCmd {
        /// Catalog a wasm block or bundle under name@version.
        Add {
            /// The name@version to catalog it under.
            name_version: String,
            /// Path to the compiled .wasm block or .cfbundle to catalog.
            path: PathBuf,
        },
        /// List everything in the catalog.
        List,
        /// Show one entry's cached signature.
        Show {
            /// The name@version to show.
            name_version: String,
        },
        /// Remove an entry from the catalog (index only; the blob remains).
        Rm {
            /// The name@version to remove.
            name_version: String,
        },
    }

    /// Parse arguments and carry out the requested command.
    pub async fn main() -> anyhow::Result<()> {
        match Cli::parse().command {
            Cmd::Specs { endpoint } => crate::daemon::specs(&endpoint).await,
            Cmd::Run {
                endpoint,
                spec,
                input,
            } => crate::daemon::run(&endpoint, &spec, &input).await,
            Cmd::Catalog { action } => catalog_cmd(action),
            Cmd::Build { spec, output } => build_cmd(&spec, output),
        }
    }

    fn catalog_cmd(action: CatalogCmd) -> anyhow::Result<()> {
        use cuttlefish_host::catalog::Catalog;

        let catalog_root = cuttlefish_host::catalog::default_root()
            .context("could not determine home directory; set CUTTLEFISH_HOME")?;
        let catalog = Catalog::open(catalog_root);

        match action {
            CatalogCmd::Add { name_version, path } => {
                let engine = wasmtime::Engine::default();
                let outcome = catalog.add(&name_version, &path, &engine)?;
                println!(
                    "catalogued {}  ({})",
                    outcome.name_version, outcome.signature
                );
                if outcome.is_permissive_default {
                    println!(
                        "warning: {} did not declare a signature (no cf_signature export \
                         present) — cached as the permissive default, which means \
                         pipeline::check will accept it next to almost anything. Add a \
                         signature() impl (see cuttlefish-sdk's Block trait) if this block \
                         has a real input/output shape.",
                        outcome.name_version
                    );
                }
                Ok(())
            }
            CatalogCmd::List => {
                // Pad to a fixed column, but never to nothing: a name at or
                // past the column width would otherwise run straight into its
                // signature, leaving a row that cannot be split back apart.
                const NAME_COLUMN: usize = 24;
                const MIN_GAP: usize = 2;
                for (name_version, entry) in catalog.list()? {
                    let gap = NAME_COLUMN
                        .saturating_sub(name_version.chars().count())
                        .max(MIN_GAP);
                    println!("{name_version}{:gap$}{}", "", entry.signature);
                }
                Ok(())
            }
            CatalogCmd::Show { name_version } => {
                let entry = catalog.show(&name_version)?;
                println!("{name_version}");
                println!("  kind:      {:?}", entry.kind);
                println!("  signature: {}", entry.signature);
                println!("  hash:      {}", entry.hash);
                println!("  created:   {}", entry.created_at);
                Ok(())
            }
            CatalogCmd::Rm { name_version } => {
                catalog.rm(&name_version)?;
                println!("removed {name_version}");
                Ok(())
            }
        }
    }

    fn build_cmd(spec_path: &Path, output: Option<PathBuf>) -> anyhow::Result<()> {
        let src = std::fs::read_to_string(spec_path)
            .with_context(|| format!("reading {}", spec_path.display()))?;
        let spec = cuttlefish_core::spec::parse_spec(&src)
            .with_context(|| format!("parsing {}", spec_path.display()))?;
        let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));

        let out_path = output.unwrap_or_else(|| spec_path.with_extension("cfbundle"));
        // Checked before any seam-checking output is printed: a spec that
        // already ends in `.cfbundle` (or an explicit `-o` pointing back at
        // it) would otherwise have its own source overwritten with the build
        // output. `out_path` can't be the same already-existing file as
        // `spec_path` unless `canonicalize` resolves both to it, since a
        // not-yet-existing `out_path` cannot be the file we just read.
        if std::fs::canonicalize(&out_path).ok() == std::fs::canonicalize(spec_path).ok() {
            bail!(
                "refusing to build: output path {} is the same file as the spec being built",
                out_path.display()
            );
        }

        let catalog_root = cuttlefish_host::catalog::default_root()
            .context("could not determine home directory; set CUTTLEFISH_HOME")?;
        let catalog = cuttlefish_host::catalog::Catalog::open(catalog_root);
        let engine = wasmtime::Engine::default();

        // cuttlefish build packages a Checked pipeline into a linear .cfbundle
        // node array (crates/cuttlefish-host/src/bundle.rs) — it doesn't yet
        // know how to encode branches, loops, or fan-in into that format. A
        // spec whose graph is a simple chain (each node has at most one
        // predecessor, no repeat_until, no branches referencing it) still
        // builds exactly as before; anything else is a clear, explicit refusal
        // rather than a silently wrong or truncated bundle.
        if !cuttlefish_core::graph::is_simple_chain(&spec.nodes, &spec.branches) {
            bail!(
                "`{}`'s graph isn't a simple linear chain (it has fan-in, a repeat_until \
                 loop, or conditional dispatch) — `cuttlefish build` doesn't yet support \
                 packaging that into a bundle. Run it via cuttlefishd instead.",
                spec.name
            );
        }
        // A confirmed-linear graph's topological order is just its declaration
        // order for a chain (each node's sole predecessor is the previous one);
        // `spec.nodes.nodes` is already in that order (NodeGraph preserves
        // insertion order — see graph.rs).
        let resolved: Vec<_> = spec
            .nodes
            .nodes
            .iter()
            .map(|(_, node)| {
                cuttlefish_host::pipeline::resolve_and_load(
                    &catalog,
                    spec_dir,
                    &node.block.to_string_lossy(),
                    cuttlefish_host::catalog::ResolutionContext::Interactive,
                )
            })
            .collect::<Result<_, _>>()
            .with_context(|| format!("resolving the pipeline for `{}`", spec.name))?;
        let checked = cuttlefish_host::pipeline::check(&engine, &resolved)
            .with_context(|| format!("checking the pipeline for `{}`", spec.name))?;

        for stage in checked.stages() {
            println!(
                "checking node `{}`      ... ok  ({})",
                stage.name, stage.signature
            );
        }

        let bytes = cuttlefish_host::bundle::build(&checked);
        std::fs::write(&out_path, &bytes)
            .with_context(|| format!("writing {}", out_path.display()))?;

        println!(
            "built: {}  ({} nodes, accepts {}, produces {})",
            out_path.display(),
            checked.stages().len(),
            checked.input(),
            checked.output()
        );
        Ok(())
    }
}

/// Talking to the daemon over whichever machine-local transport this platform
/// has: a unix socket, or a named pipe on Windows.
mod daemon {
    use anyhow::{bail, Context};
    use std::path::Path;
    use std::time::Duration;

    /// How long to wait between status polls.
    ///
    /// Short enough to feel immediate, long enough not to spin. The daemon also
    /// offers a streaming endpoint; this polls because a result is retained, so
    /// watching the stream is not required in order to observe one.
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// Build a client bound to the daemon's endpoint.
    ///
    /// reqwest gives both transports the same shape — `unix_socket` on unix,
    /// `windows_named_pipe` on Windows — so this is a `cfg` over one builder
    /// call rather than a second client implementation.
    ///
    /// Pass `endpoint` itself, not `&endpoint`: reqwest's sealed provider
    /// traits cover `&Path` and `PathBuf` but not `&PathBuf`, and no deref
    /// coercion happens at an `impl Trait` parameter.
    fn client(endpoint: &Path) -> anyhow::Result<reqwest::Client> {
        let builder = reqwest::Client::builder();
        #[cfg(unix)]
        let builder = builder.unix_socket(endpoint);
        #[cfg(windows)]
        let builder = builder.windows_named_pipe(endpoint);
        builder.build().context("building the daemon client")
    }

    pub async fn specs(socket: &Path) -> anyhow::Result<()> {
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

    pub async fn run(socket: &Path, spec: &str, input: &str) -> anyhow::Result<()> {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::main().await
}
