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
//! the daemon (`run`, `submit`, `jobs`, `resume`, `cancel`, `shutdown`,
//! `specs`). The split is by dependency, and it is no longer a platform
//! boundary: both halves compile everywhere now that the transport has a
//! named-pipe implementation on Windows.
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
        /// Submit a job without waiting for it to finish. Prints the job_id.
        Submit {
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
        /// List every job the daemon knows about, including any Interrupted
        /// ones from a prior crash.
        Jobs {
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
        },
        /// Resume a job the daemon reports as Interrupted.
        Resume {
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
            /// The job to resume.
            job_id: String,
        },
        /// Cancel a running (or interrupted) job.
        Cancel {
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
            /// The job to cancel.
            job_id: String,
        },
        /// Ask the daemon to stop, gracefully, once any in-flight request
        /// finishes.
        Shutdown {
            /// Where the daemon listens: a unix socket path, or a named
            /// pipe on Windows. Defaults per platform.
            ///
            /// `--socket` is kept as an alias so existing scripts and docs
            /// keep working; the name is simply wrong on Windows.
            #[arg(long, alias = "socket", default_value_os_t = cuttlefishd_endpoint())]
            endpoint: PathBuf,
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
        /// Scaffold a new proc block. Purely local — no running daemon
        /// required.
        Block {
            #[command(subcommand)]
            action: BlockCmd,
        },
        /// Validate a JSON value against a JSON Schema. Purely local — no
        /// running daemon required. Exit 0 and silent on success; exit 1
        /// and every violation on stderr on failure. Meant for a driver
        /// script to check a job's result (or a Rhai script's parsed
        /// infer() reply, passed back up as that job's own output) against
        /// a schema stronger than the block's declared `Ty` signature can
        /// express.
        ValidateJson {
            /// Path to the JSON Schema file.
            schema: PathBuf,
            /// The JSON value to validate, as a literal string. Reads from
            /// stdin instead if omitted, so a job's result can be piped
            /// straight in.
            #[arg(long)]
            input: Option<String>,
        },
    }

    /// `cuttlefish block` subcommands.
    #[derive(Subcommand)]
    enum BlockCmd {
        /// Scaffold a new proc block — a Rhai script by default, or a real
        /// Rust crate with `--lang rust`.
        New {
            /// The block's name — becomes its directory under
            /// `.cuttlefish/blocks/` and, for the Rust path, its Cargo
            /// crate name (`cf-block-<name>`).
            name: String,
            /// What the block accepts.
            #[arg(long)]
            input: String,
            /// What the block produces.
            #[arg(long)]
            output: String,
            /// Free-text description, written into the generated crate's
            /// Cargo.toml (Rust path only).
            #[arg(long)]
            description: Option<String>,
            /// `rhai` (default, no toolchain needed) or `rust` (a real
            /// crate, needs a Rust toolchain to build).
            #[arg(long, default_value = "rhai")]
            lang: String,
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
            Cmd::Submit {
                endpoint,
                spec,
                input,
            } => crate::daemon::submit(&endpoint, &spec, &input).await,
            Cmd::Jobs { endpoint } => crate::daemon::jobs(&endpoint).await,
            Cmd::Resume { endpoint, job_id } => crate::daemon::resume(&endpoint, &job_id).await,
            Cmd::Cancel { endpoint, job_id } => crate::daemon::cancel(&endpoint, &job_id).await,
            Cmd::Shutdown { endpoint } => crate::daemon::shutdown(&endpoint).await,
            Cmd::Catalog { action } => catalog_cmd(action),
            Cmd::Build { spec, output } => build_cmd(&spec, output),
            Cmd::Block {
                action:
                    BlockCmd::New {
                        name,
                        input,
                        output,
                        description,
                        lang,
                    },
            } => block_new_cmd(&name, &input, &output, description.as_deref(), &lang),
            Cmd::ValidateJson { schema, input } => validate_json_cmd(&schema, input.as_deref()),
        }
    }

    /// Validate a JSON value (inline `--input`, or stdin if omitted)
    /// against a JSON Schema file. All violations, not just the first —
    /// a script author fixing their prompt/schema wants the whole list,
    /// not one round trip per mistake.
    fn validate_json_cmd(schema_path: &Path, input: Option<&str>) -> anyhow::Result<()> {
        let schema_text = std::fs::read_to_string(schema_path)
            .with_context(|| format!("reading {}", schema_path.display()))?;
        let schema: serde_json::Value = serde_json::from_str(&schema_text)
            .with_context(|| format!("{} is not valid JSON", schema_path.display()))?;

        let input_text = match input {
            Some(s) => s.to_string(),
            None => {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("reading JSON value from stdin")?;
                buf
            }
        };
        let instance: serde_json::Value =
            serde_json::from_str(&input_text).context("the value to validate is not valid JSON")?;

        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            anyhow::anyhow!("{} is not a valid JSON Schema: {e}", schema_path.display())
        })?;

        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();

        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "value does not conform to {}:\n{}",
                schema_path.display(),
                errors.join("\n")
            );
        }
    }

    /// Scaffold a new proc block — a Rhai script by default, or a real
    /// Rust crate with `--lang rust`.
    fn block_new_cmd(
        name: &str,
        input: &str,
        output: &str,
        description: Option<&str>,
        lang: &str,
    ) -> anyhow::Result<()> {
        cuttlefish_host::catalog::validate_block_name(name).map_err(|e| anyhow::anyhow!("{e}"))?;

        let input_ty: cuttlefish_abi::Ty = input
            .parse()
            .map_err(|e| anyhow::anyhow!("--input `{input}` is not a valid type: {e}"))?;
        let output_ty: cuttlefish_abi::Ty = output
            .parse()
            .map_err(|e| anyhow::anyhow!("--output `{output}` is not a valid type: {e}"))?;

        let block_dir = Path::new(".cuttlefish/blocks").join(name);
        if block_dir.exists() {
            bail!("{} already exists", block_dir.display());
        }
        std::fs::create_dir_all(&block_dir)
            .with_context(|| format!("creating {}", block_dir.display()))?;

        match lang {
            "rhai" => scaffold_rhai(&block_dir, &input_ty, &output_ty)?,
            "rust" => scaffold_rust(&block_dir, name, description, &input_ty, &output_ty)?,
            other => bail!("--lang must be \"rhai\" or \"rust\", got \"{other}\""),
        }

        println!("scaffolded {} in {}", name, block_dir.display());
        Ok(())
    }

    fn scaffold_rust(
        block_dir: &Path,
        name: &str,
        description: Option<&str>,
        input: &cuttlefish_abi::Ty,
        output: &cuttlefish_abi::Ty,
    ) -> anyhow::Result<()> {
        // cuttlefish and cuttlefish-sdk share one workspace version (both
        // use `version.workspace = true`), so this is genuinely the SDK
        // version the running binary was built against, not a coincidence.
        let cuttlefish_sdk_version = env!("CARGO_PKG_VERSION");
        let description = description.unwrap_or("A cuttlefish proc block.");

        let cargo_toml = format!(
            r#"[package]
name = "cf-block-{name}"
description = "{description}"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
cuttlefish-sdk = "{cuttlefish_sdk_version}"
serde_json = "1"
"#
        );
        std::fs::write(block_dir.join("Cargo.toml"), cargo_toml)
            .with_context(|| format!("writing {}", block_dir.join("Cargo.toml").display()))?;

        // Deliberately not derived from `name` — a block name can contain
        // '-'/'_' in ways that don't map cleanly to a valid Rust
        // identifier without more string-mangling than this is worth, and
        // this struct is never referenced from outside this one generated
        // file anyway.
        let struct_name = "GeneratedBlock";
        let lib_rs = format!(
            r#"//! Generated by `cuttlefish block new`. Identity passthrough —
//! edit `step`/`start` below to make this block do something real.

use cuttlefish_sdk::{{export_block, Block, Command, Event, Signature}};

#[derive(Default)]
struct {struct_name};

impl Block for {struct_name} {{
    fn signature() -> Signature {{
        Signature {{
            input: "{input}".parse().expect("a literal type"),
            output: "{output}".parse().expect("a literal type"),
        }}
    }}

    fn start(&mut self, input: serde_json::Value) -> Command {{
        // Identity passthrough — replace with real logic. See the
        // cuttlefish-author skill for the Command/Event vocabulary
        // (Open, Slice, Infer, PageText, PageImage, Done, Fail).
        Command::Done {{ result: input }}
    }}

    fn step(&mut self, _event: Event) -> Command {{
        // Unreachable for the identity passthrough above (start() already
        // finishes via Command::Done, with no round-trip through step()) —
        // still required because Block::step has no default implementation.
        Command::Fail {{
            code: "unexpected_event".into(),
            message: "this block never issues a Command that would produce an Event".into(),
        }}
    }}
}}

export_block!({struct_name});
"#
        );
        std::fs::create_dir_all(block_dir.join("src"))
            .with_context(|| format!("creating {}", block_dir.join("src").display()))?;
        std::fs::write(block_dir.join("src/lib.rs"), lib_rs)
            .with_context(|| format!("writing {}", block_dir.join("src/lib.rs").display()))?;
        Ok(())
    }

    fn scaffold_rhai(
        block_dir: &Path,
        input: &cuttlefish_abi::Ty,
        output: &cuttlefish_abi::Ty,
    ) -> anyhow::Result<()> {
        let script = format!(
            "//! signature: {input} -> {output}\n\
             //! Generated by `cuttlefish block new`. Identity passthrough —\n\
             //! edit the expression below to make this block do something\n\
             //! real. Call `infer(prompt, max_tokens)` to invoke the model.\n\
             //! See the cuttlefish-author skill for the determinism rules\n\
             //! this script must follow (no wall-clock/randomness, no\n\
             //! try/catch around a host call).\n\
             input\n"
        );
        std::fs::write(block_dir.join("block.rhai"), script)
            .with_context(|| format!("writing {}", block_dir.join("block.rhai").display()))?;
        Ok(())
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

        // `bundle::build` has no field to carry a Script node's actual
        // script text — it copies `module_bytes` verbatim into the
        // `.cfbundle` body, which for a Script node is the shared
        // interpreter's bytes, not the script. Bundling one would silently
        // embed a redundant interpreter copy and drop the script itself.
        // Reject explicitly, before any bundle output is written, the same
        // "fail loudly on what's not supported yet" precedent cuttlefishd's
        // startup check already applies to Bundle-kind nodes.
        if let Some(stage) = checked
            .stages()
            .iter()
            .find(|s| s.kind == cuttlefish_host::catalog::ArtifactKind::Script)
        {
            bail!(
                "node `{}` is a Rhai script. `cuttlefish build` doesn't support packaging a \
                 Script node into a bundle yet — run it via cuttlefishd instead.",
                stage.name
            );
        }

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

    /// Post a job and return its `job_id`, without waiting for it to finish.
    ///
    /// Shared by `run` and `submit`: both need exactly this — submit the job,
    /// surface a rejection clearly, extract the id — and differ only in what
    /// they do once they have it (poll to completion vs. print and return).
    async fn submit_job(
        client: &reqwest::Client,
        socket: &Path,
        spec: &str,
        input: &str,
    ) -> anyhow::Result<String> {
        let input: serde_json::Value =
            serde_json::from_str(input).context("--input must be JSON")?;

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

        Ok(submitted.json::<serde_json::Value>().await?["job_id"]
            .as_str()
            .context("daemon response had no job_id")?
            .to_string())
    }

    /// Submit a job and print its `job_id` immediately, without waiting for
    /// it to finish. Unlike `run`, this never polls: a caller that wants the
    /// result later can poll `GET /jobs/{job_id}` (or watch `/events`) on its
    /// own schedule.
    pub async fn submit(socket: &Path, spec: &str, input: &str) -> anyhow::Result<()> {
        let job_id = submit_job(&client(socket)?, socket, spec, input).await?;
        println!("{job_id}");
        Ok(())
    }

    /// List every job the daemon knows about — the raw pretty-printed `GET
    /// /jobs` response, same convention `specs` already uses: no derived
    /// one-line summary formatter, since that's not what's been asked for.
    pub async fn jobs(socket: &Path) -> anyhow::Result<()> {
        let body: serde_json::Value = client(socket)?
            .get("http://localhost/jobs")
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?
            .json()
            .await?;

        println!("{}", serde_json::to_string_pretty(&body)?);
        Ok(())
    }

    /// Resume a job the daemon reports as `Interrupted`. Deliberately not
    /// automatic — the daemon rejects a resume of anything else, and that
    /// rejection is surfaced here rather than swallowed.
    pub async fn resume(socket: &Path, job_id: &str) -> anyhow::Result<()> {
        let resp = client(socket)?
            .post(format!("http://localhost/jobs/{job_id}/resume"))
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "daemon rejected the resume: {status} {}",
                resp.text().await?
            );
        }

        println!("resuming {job_id}");
        Ok(())
    }

    /// Cancel a running (or interrupted) job.
    pub async fn cancel(socket: &Path, job_id: &str) -> anyhow::Result<()> {
        let resp = client(socket)?
            .delete(format!("http://localhost/jobs/{job_id}"))
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;

        if !resp.status().is_success() {
            bail!("daemon rejected the cancel: {}", resp.status());
        }

        println!("cancelled {job_id}");
        Ok(())
    }

    /// Ask the daemon to stop, gracefully, once any in-flight request
    /// finishes.
    pub async fn shutdown(socket: &Path) -> anyhow::Result<()> {
        client(socket)?
            .post("http://localhost/shutdown")
            .send()
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;

        println!("shutdown requested");
        Ok(())
    }

    pub async fn run(socket: &Path, spec: &str, input: &str) -> anyhow::Result<()> {
        let client = client(socket)?;
        let job_id = submit_job(&client, socket, spec, input).await?;

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
