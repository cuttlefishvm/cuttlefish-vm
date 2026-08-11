//! `cuttlefish dev` — own the daemon lifecycle so no project has to.
//!
//! Every project that drives cuttlefish from a script ends up writing the
//! same hundred-odd lines of bash: hash the project path to get an endpoint,
//! check whether a recorded daemon is still alive, notice when it is serving
//! a stale spec, restart it, wait for it to come up, resume anything a crash
//! left mid-flight. That procedure was documented rather than shipped, so
//! every project reimplemented it slightly differently.
//!
//! This is that procedure, as a command:
//!
//! ```text
//! cuttlefish dev --spec pipeline.cuttlefish -- run --spec pipeline --input '{}'
//! ```
//!
//! Everything after `--` is handed to the ordinary client with `--endpoint`
//! filled in, so anything `cuttlefish` can do works unchanged.
//!
//! # Why the endpoint is hashed rather than placed in the project
//!
//! A unix socket path is capped at roughly 104 bytes by `sun_path`, and a
//! project a few directories deep blows straight through that. So the socket
//! lives under `TMPDIR`, named by a hash of the *resolved* project path —
//! resolved, so that reaching the same project through a symlink and through
//! its real path finds one daemon rather than two.
//!
//! # Why the spec's contents are fingerprinted, not just its path
//!
//! A daemon loads and typechecks its pipeline once, at startup. Editing a
//! spec in place therefore leaves a running daemon serving the *previous*
//! graph, with nothing to indicate it. Comparing paths alone misses this
//! entirely — the path is unchanged, which is exactly the case that bites.
//! So the record carries a hash of the spec's bytes, and a changed hash
//! restarts the daemon.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// What `dev` remembers about the daemon it started.
#[derive(serde::Serialize, serde::Deserialize)]
struct DaemonRecord {
    pid: u32,
    spec_path: String,
    /// SHA-256 of the spec's bytes at the moment the daemon was started —
    /// see the module docs on why the path alone is not enough.
    spec_hash: String,
    endpoint: String,
}

/// Ensure a daemon is serving `spec`, then run `args` against it.
pub async fn run(spec: &Path, args: &[String]) -> anyhow::Result<()> {
    let endpoint = ensure(spec).await?;

    if args.is_empty() {
        println!("{}", endpoint.display());
        return Ok(());
    }

    let status = std::process::Command::new(std::env::current_exe()?)
        .args(args)
        .arg("--endpoint")
        .arg(&endpoint)
        .status()
        .context("running the client against the ensured daemon")?;

    // Exit codes are the machine-readable result (0 completed, 1 failed,
    // 2 cancelled), so they have to survive this wrapper — swallowing them
    // would make every `dev` invocation look successful.
    std::process::exit(status.code().unwrap_or(1));
}

/// Start a daemon for `spec` if one isn't already serving exactly it.
pub async fn ensure(spec: &Path) -> anyhow::Result<PathBuf> {
    let spec_path =
        std::fs::canonicalize(spec).with_context(|| format!("no such spec: {}", spec.display()))?;
    let project = std::env::current_dir()?;
    let project = std::fs::canonicalize(&project).unwrap_or(project);
    let state = project.join(".cuttlefish");
    std::fs::create_dir_all(state.join("jobs"))?;

    let endpoint = endpoint_for(&project);
    let hash = hash_of(&spec_path)?;
    let record_path = state.join("daemon.json");

    if let Some(record) = read_record(&record_path) {
        let recorded = PathBuf::from(&record.endpoint);
        if alive(&recorded).await {
            if record.spec_path == spec_path.to_string_lossy() && record.spec_hash == hash {
                auto_resume(&recorded).await?;
                return Ok(recorded);
            }
            // Either a different spec or the same spec with different
            // contents. A daemon serves one graph, loaded at startup, so
            // both mean stop and start again.
            eprintln!("spec changed; restarting the daemon");
            shutdown(&recorded).await;
        }
        // A recorded PID that no longer answers may have been recycled by
        // an unrelated process, so the record is discarded rather than
        // signalled.
        let _ = std::fs::remove_file(&record_path);
    }

    let child = std::process::Command::new("cuttlefishd")
        .arg(&spec_path)
        .arg(&endpoint)
        .env("CUTTLEFISH_JOBS_HOME", state.join("jobs"))
        .stdout(std::fs::File::create(state.join("daemon.log"))?)
        .stderr(std::process::Stdio::from(
            std::fs::File::options()
                .append(true)
                .open(state.join("daemon.log"))?,
        ))
        .spawn()
        .context(
            "starting cuttlefishd — is it on PATH? If not, resolve the binaries first \
             (see the cuttlefish-cli skill or the cuttlefish-binary-resolver agent)",
        )?;

    for _ in 0..100 {
        if alive(&endpoint).await {
            write_record(
                &record_path,
                &DaemonRecord {
                    pid: child.id(),
                    spec_path: spec_path.to_string_lossy().into_owned(),
                    spec_hash: hash,
                    endpoint: endpoint.to_string_lossy().into_owned(),
                },
            )?;
            auto_resume(&endpoint).await?;
            return Ok(endpoint);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let log = std::fs::read_to_string(state.join("daemon.log")).unwrap_or_default();
    anyhow::bail!(
        "cuttlefishd did not come up for {}. Its log says:\n{log}",
        spec_path.display()
    )
}

/// Stop this project's daemon, if it has one.
pub async fn stop() -> anyhow::Result<()> {
    let project = std::env::current_dir()?;
    let record_path = project.join(".cuttlefish/daemon.json");
    match read_record(&record_path) {
        Some(record) => {
            shutdown(&PathBuf::from(&record.endpoint)).await;
            let _ = std::fs::remove_file(&record_path);
            println!("stopped");
        }
        None => println!("no daemon recorded for this project"),
    }
    Ok(())
}

/// `$TMPDIR/cf-<16 hex>.sock` — short enough for `sun_path`, unique per
/// resolved project path.
fn endpoint_for(project: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(project.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir().join(format!("cf-{short}.sock"))
}

/// Fingerprint the spec *and* every block it references by path.
///
/// The spec alone is not enough, and the gap is silent. A daemon loads and
/// typechecks its whole pipeline once, at startup — script text included —
/// so editing a block leaves the daemon happily serving the previous
/// version of it. Nothing errors; the run simply reflects code that is no
/// longer on disk, which is the worst way to lose an afternoon.
///
/// Blocks are found by parsing the spec rather than by scanning for files,
/// so this tracks exactly what the pipeline actually uses. Catalogued
/// `name@version` entries are skipped deliberately: those are immutable by
/// construction, so there is nothing about them that can go stale.
fn hash_of(spec_path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let text = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());

    // A spec that doesn't parse is the daemon's problem to report, not
    // this one's: fall back to hashing the spec alone so `dev` still
    // restarts and lets the real error surface with its real message.
    if let Ok(spec) = cuttlefish_core::spec::parse_spec(&text) {
        let dir = spec_path.parent().unwrap_or(Path::new("."));
        let mut referenced: Vec<PathBuf> = spec
            .nodes
            .nodes
            .iter()
            .map(|(_, node)| dir.join(&node.block))
            .filter(|p| p.exists())
            .collect();
        // Sorted so the fingerprint doesn't depend on map iteration order.
        referenced.sort();
        for path in referenced {
            if let Ok(bytes) = std::fs::read(&path) {
                hasher.update(path.as_os_str().as_encoded_bytes());
                hasher.update(&bytes);
            }
        }
    }

    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn read_record(path: &Path) -> Option<DaemonRecord> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_record(path: &Path, record: &DaemonRecord) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(record)?)?;
    Ok(())
}

async fn alive(endpoint: &Path) -> bool {
    crate::daemon::specs_quiet(endpoint).await
}

async fn shutdown(endpoint: &Path) {
    crate::daemon::shutdown_quiet(endpoint).await;
    for _ in 0..50 {
        if !alive(endpoint).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Resume anything a previous crash left mid-flight.
///
/// A rejection because the job is no longer interrupted means another
/// session got there first — an expected race, not a failure. Anything else
/// is surfaced.
async fn auto_resume(endpoint: &Path) -> anyhow::Result<()> {
    let interrupted = crate::daemon::interrupted_jobs(endpoint)
        .await
        .unwrap_or_default();
    for job in interrupted {
        eprintln!("resuming interrupted job {job}");
        if let Err(e) = crate::daemon::resume(endpoint, &job).await {
            let message = e.to_string();
            if !message.contains("409") {
                return Err(e);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_is_short_enough_for_sun_path_however_deep_the_project() {
        // The reason the socket is not placed inside the project: sun_path
        // caps around 104 bytes, and a deep project blows past it.
        let deep = PathBuf::from("/Users/someone/very/deeply/nested".repeat(8));
        let endpoint = endpoint_for(&deep);
        assert!(
            endpoint.as_os_str().len() < 104,
            "{} is {} bytes",
            endpoint.display(),
            endpoint.as_os_str().len()
        );
    }

    #[test]
    fn one_project_gets_one_endpoint_and_two_projects_get_two() {
        let a = endpoint_for(Path::new("/tmp/project-a"));
        let b = endpoint_for(Path::new("/tmp/project-b"));
        assert_eq!(a, endpoint_for(Path::new("/tmp/project-a")));
        assert_ne!(a, b);
    }

    #[test]
    fn editing_a_referenced_block_changes_the_fingerprint() {
        // The failure this caught in practice: the spec is untouched, the
        // block is edited, and a daemon that only watched the spec kept
        // serving the old script with nothing to indicate it. The run
        // reflects code no longer on disk.
        let dir = tempfile::tempdir().unwrap();
        let block = dir.path().join("check.rhai");
        std::fs::write(&block, "//! signature: json -> json\ninput\n").unwrap();
        let spec = dir.path().join("p.cuttlefish");
        std::fs::write(
            &spec,
            r#"spec p = {
  description = "Use when testing.";
  model = Stub "x";
  data_policy = Local_only;
  capabilities = [ ];
  nodes = { check = { block = "./check.rhai"; }; };
}
"#,
        )
        .unwrap();

        let before = hash_of(&spec).unwrap();
        std::fs::write(&block, "//! signature: json -> json\n#{ edited: true }\n").unwrap();
        assert_ne!(
            before,
            hash_of(&spec).unwrap(),
            "a block edit must change the fingerprint, or the daemon serves stale code"
        );
    }

    #[test]
    fn editing_a_spec_in_place_changes_its_hash() {
        // The case comparing paths alone misses, and the one that actually
        // bites: same path, different graph, daemon still serving the old
        // one with nothing to show for it.
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("p.cuttlefish");
        std::fs::write(&spec, "spec a = { }").unwrap();
        let before = hash_of(&spec).unwrap();
        std::fs::write(&spec, "spec a = { nodes = {}; }").unwrap();
        assert_ne!(before, hash_of(&spec).unwrap());
    }

    #[test]
    fn a_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        write_record(
            &path,
            &DaemonRecord {
                pid: 42,
                spec_path: "/tmp/p.cuttlefish".into(),
                spec_hash: "abc".into(),
                endpoint: "/tmp/cf-x.sock".into(),
            },
        )
        .unwrap();
        let back = read_record(&path).expect("a written record must read back");
        assert_eq!(back.pid, 42);
        assert_eq!(back.spec_hash, "abc");
        // A corrupt record must read as absent rather than blowing up: it
        // is a cache, and the recovery is simply to start a daemon.
        std::fs::write(&path, "not json").unwrap();
        assert!(read_record(&path).is_none());
    }
}
