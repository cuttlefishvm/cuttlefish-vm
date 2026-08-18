//! Downloading a URL into the job's directory.
//!
//! A corpus that lives on the web is still a corpus. Without this, every job
//! reading public data has to be preceded by a hand-written download script
//! — which is what real users did: 120 lines of Python to pull CMS listings
//! before cuttlefish saw a single byte. That work is outside the pipeline,
//! so it gets none of what the pipeline provides: no capability check, no
//! per-item failure isolation, no resume, no ledger.
//!
//! The result is a *file in the job directory*, opened as an ordinary
//! handle. That is the whole design: a fetched resource is indistinguishable
//! downstream from a local one, so `slice`, `identify`, `document_text`,
//! `page_image` and the rest work on it with no further changes.

use std::path::{Path, PathBuf};

/// Largest response this will keep.
///
/// A URL is attacker-influenced in a way a local path is not: a spec author
/// grants a prefix, and what sits behind it can change size without warning.
/// The cap is checked *while* streaming rather than after, so a response
/// claiming to be modest and then continuing forever is stopped rather than
/// discovered once the disk is full.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// How long to wait for the whole response.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Fetch `url` into `job_dir/fetched/`, returning the file's path.
pub async fn fetch_to_file(url: &str, job_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = job_dir.join("fetched");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;

    let target = dir.join(file_name_for(url));

    // A URL already fetched by this job is not fetched twice. This matters
    // more than it looks: a Rhai script is replayed from the top for each
    // host-call answer, and a fan-out re-runs items after a resume — so
    // without this, one logical read becomes many requests against somebody
    // else's server.
    if target.exists() {
        return Ok(target);
    }

    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // Named so the other end can tell what is calling and throttle or
        // block it deliberately rather than guessing.
        .user_agent(concat!("cuttlefish/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow::anyhow!("building the HTTP client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("fetching {url}: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        // The status is the whole diagnosis for a fetch: 404 means the URL is
        // wrong, 403 means the grant is fine and the server declined, and
        // conflating them sends the reader to the wrong place.
        anyhow::bail!("fetching {url}: server returned {status}");
    }

    // Streamed rather than `bytes()`, so the ceiling can stop a response
    // mid-flight instead of after it has already been held in memory.
    let mut written: u64 = 0;
    let mut out = std::fs::File::create(&target)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", target.display()))?;
    let mut stream = response;

    loop {
        let chunk = match stream.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                // A partial file left behind would later read as a cache hit
                // and be handed downstream as though it were complete.
                let _ = std::fs::remove_file(&target);
                anyhow::bail!("reading the response for {url}: {e}");
            }
        };
        written += chunk.len() as u64;
        if written > MAX_BYTES {
            let _ = std::fs::remove_file(&target);
            anyhow::bail!(
                "{url} exceeded the {MAX_BYTES}-byte fetch ceiling; \
                 it was stopped mid-download rather than filling the disk"
            );
        }
        use std::io::Write as _;
        out.write_all(&chunk)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", target.display()))?;
    }

    Ok(target)
}

/// A stable, filesystem-safe name for a URL.
///
/// Hashed rather than derived from the path, because two URLs can share a
/// last path segment and a query string is part of the identity — deriving
/// from either would collide, and a collision means one fetch silently
/// serving another's bytes. The suffix is kept when there is one, purely so
/// a person looking in the job directory can tell a PDF from a page.
fn file_name_for(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();

    let extension = url
        .rsplit('/')
        .next()
        .and_then(|last| last.split(['?', '#']).next())
        .and_then(|last| last.rsplit_once('.'))
        .map(|(_, ext)| ext)
        .filter(|ext| ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("");

    if extension.is_empty() {
        short
    } else {
        format!("{short}.{extension}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_stable_and_keeps_a_useful_suffix() {
        let a = file_name_for("https://x.org/docs/report.pdf");
        assert_eq!(a, file_name_for("https://x.org/docs/report.pdf"));
        assert!(a.ends_with(".pdf"), "{a}");
    }

    #[test]
    fn urls_sharing_a_last_segment_do_not_collide() {
        // The failure this guards: two fetches writing the same file, so one
        // silently serves the other's bytes.
        let a = file_name_for("https://x.org/2024/data.json");
        let b = file_name_for("https://x.org/2025/data.json");
        assert_ne!(a, b);
    }

    #[test]
    fn a_query_string_is_part_of_the_identity() {
        assert_ne!(
            file_name_for("https://x.org/list?page=1"),
            file_name_for("https://x.org/list?page=2"),
        );
    }

    #[test]
    fn a_url_with_no_sensible_suffix_still_gets_a_name() {
        let n = file_name_for("https://x.org/transmittals");
        assert!(!n.is_empty() && !n.contains('/'), "{n}");
        // A path segment that is not really an extension must not become one.
        assert!(!file_name_for("https://x.org/a.verylongextension").contains('.'));
    }
}
