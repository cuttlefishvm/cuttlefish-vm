//! `cuttlefish init` — scaffold a working project, not an empty one.
//!
//! # Why this exists
//!
//! Watching a real agent start from nothing, it wrote 864 lines of harness
//! before running a single job: daemon lifecycle, fixture generation,
//! assertion plumbing, project layout. Most of that is the same in every
//! project, and the parts that differ are the parts nobody should be
//! guessing at.
//!
//! Two of the defaults here are deliberate corrections to what that agent
//! reached for on its own.
//!
//! **Blocks are referenced by path, not catalogued.** A catalogued
//! `name@version` is immutable all the way down — `catalog rm` drops the
//! index entry but the blob stays, so a version can never be re-pointed at
//! different bytes. That is exactly right for anything a shipped spec
//! depends on, and exactly wrong for edit-run-edit. Faced with it, the agent
//! built content-hash versions and generated specs to route around it.
//! It never needed to: a spec entry that exists on disk resolves as a direct
//! path (see `catalog::resolve`), so `block = "./blocks/thing.rhai"` just
//! works and an edit is picked up on the next run. Catalog when you ship,
//! not while you build.
//!
//! **The test harness asserts in both directions.** A scaffold that only
//! checks "did the expected finding appear" teaches a habit that hides the
//! more embarrassing failure: a checker that flags everything passes such a
//! test perfectly. The generated harness fails on a missed finding *and* on
//! one that was never injected.

use anyhow::Context;
use std::path::Path;

/// Files written by `init`, as (relative path, contents).
///
/// Built as data rather than written inline so the whole layout can be
/// asserted in a test without touching a filesystem, and so a dry run can
/// list exactly what would be created.
pub fn files(name: &str) -> Vec<(String, String)> {
    vec![
        (format!("{name}.cuttlefish"), spec(name)),
        ("blocks/check.rhai".into(), block()),
        ("schemas/report.json".into(), schema()),
        ("tests/run.sh".into(), harness(name)),
        ("tests/assert.py".into(), assertions()),
        (".gitignore".into(), gitignore()),
        ("README.md".into(), readme(name)),
    ]
}

/// Write the scaffold into `dir`, refusing to clobber anything.
pub fn run(dir: &Path, name: &str) -> anyhow::Result<()> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.is_empty()
    {
        anyhow::bail!(
            "`{name}` isn't usable as a spec name — use letters, digits, `_` or `-`. \
             It becomes the identifier in `spec <name> = {{ ... }}`."
        );
    }

    let planned = files(name);

    // Check every target first, so a collision cannot leave a half-written
    // project behind. Overwriting is never the right default here: these are
    // files someone edits, and silently replacing an edited harness is worse
    // than refusing.
    let existing: Vec<&str> = planned
        .iter()
        .map(|(path, _)| path.as_str())
        .filter(|path| dir.join(path).exists())
        .collect();
    if !existing.is_empty() {
        anyhow::bail!(
            "refusing to overwrite existing file(s): {}. \
             `init` scaffolds a new project; to add one block to an existing one, use \
             `cuttlefish block new`.",
            existing.join(", ")
        );
    }

    for (path, contents) in &planned {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, contents)
            .with_context(|| format!("writing {}", target.display()))?;
    }

    // The harness is meant to be run, so make it runnable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join("tests/run.sh");
        let mut perms = std::fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms)?;
    }

    println!("scaffolded `{name}` in {}", dir.display());
    for (path, _) in &planned {
        println!("  {path}");
    }
    println!("\nnext: ./tests/run.sh");
    Ok(())
}

fn spec(name: &str) -> String {
    format!(
        r#"spec {name} = {{
  description = "Use when checking a JSONL corpus for structural problems.";
  model = Stub "unused";
  data_policy = Local_only;
  capabilities = [ Read "./corpus", Read "./schemas", Read "./blocks" ];
  nodes = {{
    check = {{
      # A path, not a catalog name, and that is the point: a catalogued
      # name@version is immutable all the way down, so editing a block
      # means inventing a new version every time. A path that exists on
      # disk resolves directly, so an edit is simply picked up on the next
      # run. Catalog it when you ship it.
      block   = "./blocks/check.rhai";
      over    = "./corpus/manifest.jsonl";
      accept  = [ Schema "./schemas/report.json" ];
      on_fail = [ retry 1, escalate ];
    }};
  }};
}}
"#
    )
}

fn block() -> String {
    r#"//! signature: json -> json
//! Checks one item and reports what is wrong with it.
//!
//! Front-load any open/slice calls: the interpreter replays this whole
//! script from the top for each host-call answer, so work done after a
//! host call is redone on every subsequent one. Pure computation last.

let problems = [];

if type_of(input.id) != "string" || input.id == "" {
    problems.push(#{ class: "missing_id", detail: "no usable id" });
}
if type_of(input.value) == "()" {
    problems.push(#{ class: "missing_value", detail: "value is absent" });
}

// `ok` is `json` rather than a bool: bool is not a spec type. The accept
// schema is what pins it to an actual boolean.
#{ id: input.id, ok: problems.is_empty(), problems: problems }
"#
    .to_string()
}

fn schema() -> String {
    r#"{
  "type": "object",
  "required": ["id", "ok", "problems"],
  "properties": {
    "id": { "type": "string" },
    "ok": { "type": "boolean" },
    "problems": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["class", "detail"],
        "properties": {
          "class": { "type": "string" },
          "detail": { "type": "string" }
        }
      }
    }
  }
}
"#
    .to_string()
}

fn harness(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# Build a fixture with known faults, run the pipeline over it, and assert
# the findings match exactly.
#
# No daemon bookkeeping lives here on purpose. `cuttlefish dev` owns the
# lifecycle -- starting one daemon per project, noticing a stale one, and
# restarting when the spec changes. Hand-rolling that is how a project ends
# up with a hundred lines of bash that drift from every other project's.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 tests/make_fixture.py 2>/dev/null || {{
  mkdir -p corpus
  # Three good rows and two deliberately broken ones. The broken rows are
  # the point: a pipeline that only ever sees clean input proves nothing.
  cat > corpus/manifest.jsonl <<'JSONL'
{{"id": "a1", "value": 10}}
{{"id": "a2", "value": 20}}
{{"id": "", "value": 30}}
{{"id": "a4"}}
{{"id": "a5", "value": 50}}
JSONL
}}

cuttlefish dev --spec {name}.cuttlefish -- \
  run --spec {name} --input '{{}}' > tests/last-run.json

python3 tests/assert.py tests/last-run.json
"#
    )
}

fn assertions() -> String {
    r#"#!/usr/bin/env python3
"""Assert the run reported exactly the faults the fixture injected.

Exactly, in both directions. A fault that went unreported is a miss; a
fault reported that was never injected is a false positive. Both fail --
a checker that flags everything passes a one-directional test perfectly,
which is why this scaffold asserts both from the very first run.
"""
import json
import sys

EXPECTED = {"missing_id", "missing_value"}

envelope = json.load(open(sys.argv[1]))
if envelope.get("status") != "completed":
    sys.exit(f"job did not complete: {json.dumps(envelope, indent=2)}")

result = envelope["result"]
print(f"succeeded={result['succeeded']} failed={result['failed']}")

found = set()
with open(result["results_path"]) as handle:
    for line in handle:
        if not line.strip():
            continue
        for problem in json.loads(line)["result"].get("problems", []):
            found.add(problem["class"])

missed = EXPECTED - found
extra = found - EXPECTED
if missed:
    sys.exit(f"FAIL: injected but never reported: {sorted(missed)}")
if extra:
    sys.exit(f"FAIL: reported but never injected: {sorted(extra)}")
print(f"OK: reported exactly {sorted(EXPECTED)}")
"#
    .to_string()
}

fn gitignore() -> String {
    r#"# Per-project daemon state, job ledgers, and results.
.cuttlefish/
tests/last-run.json
out/
"#
    .to_string()
}

fn readme(name: &str) -> String {
    format!(
        r#"# {name}

```bash
./tests/run.sh
```

## Layout

| | |
|---|---|
| `{name}.cuttlefish` | the spec: what runs, over what, and what "done" means |
| `blocks/check.rhai` | the block, referenced **by path** so edits take effect immediately |
| `schemas/report.json` | the `accept` contract, checked per item |
| `tests/` | fixture with known faults, plus an assertion in both directions |

## Things that will otherwise cost you an afternoon

**Catalog when you ship, not while you build.** A catalogued `name@version`
is immutable all the way down: `catalog rm` drops the index entry but the
blob stays, so a version can never be re-pointed at different bytes. A spec
entry that exists on disk resolves as a path instead, which is why
`blocks/check.rhai` is referenced the way it is. Reach for `catalog add`
when the block is stable and something else needs to depend on it.

**A Rhai script is replayed from the top for every host-call answer.** Call
`open`/`slice`/`infer` early and do pure computation after, or the pure part
runs again for each host call.

**`bool` is not a spec type.** Declare a boolean field as `json` and pin it
to an actual boolean in the accept schema, as `report.json` does.

**A missing file surfaces as `capability_denied`, not "not found."** The
capability check happens before the open, so a typo'd path and an ungranted
one look identical. Check the path before suspecting the grant.

**One daemon serves one spec.** `cuttlefish dev` restarts it when the spec
changes; a reused daemon would keep serving the previous graph.

## When something gives up

```bash
cuttlefish escalations                        # what was abandoned, and why
cuttlefish escalations --manifest retry.jsonl # hand that work back
```

An escalation means the `on_fail` ladder was exhausted, so re-running it
unchanged fails identically. Point a new spec at `retry.jsonl` with
something actually different -- a stronger model, a looser schema, a fixed
block.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scaffold_references_its_block_by_path_not_by_catalog_name() {
        // The correction this scaffold exists to make. A catalogued version
        // is immutable, so a scaffold that catalogued its block would teach
        // every new project the edit-run-edit workaround instead of the
        // thing that just works.
        let spec = files("demo")
            .into_iter()
            .find(|(p, _)| p == "demo.cuttlefish")
            .unwrap()
            .1;
        assert!(
            spec.contains(r#"block   = "./blocks/check.rhai""#),
            "{spec}"
        );
        assert!(
            !spec.contains("check@"),
            "the scaffold must not catalog its own block: {spec}"
        );
    }

    #[test]
    fn the_generated_harness_asserts_in_both_directions() {
        // A one-directional check passes for a block that flags everything,
        // which is the failure this scaffold should not teach.
        let asserts = files("demo")
            .into_iter()
            .find(|(p, _)| p == "tests/assert.py")
            .unwrap()
            .1;
        assert!(asserts.contains("injected but never reported"), "{asserts}");
        assert!(asserts.contains("reported but never injected"), "{asserts}");
    }

    #[test]
    fn the_fixture_contains_faults_and_the_schema_pins_the_boolean() {
        let all = files("demo");
        let harness = &all.iter().find(|(p, _)| p == "tests/run.sh").unwrap().1;
        // A fixture of only-good rows proves nothing about a checker.
        assert!(harness.contains(r#"{"id": "", "value": 30}"#), "{harness}");

        let schema = &all
            .iter()
            .find(|(p, _)| p == "schemas/report.json")
            .unwrap()
            .1;
        assert!(
            schema.contains(r#""ok": { "type": "boolean" }"#),
            "bool is not a spec type, so the schema is what pins it: {schema}"
        );
    }

    #[test]
    fn init_refuses_to_clobber_and_writes_nothing_when_it_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "mine").unwrap();

        let err = run(dir.path(), "demo").expect_err("an existing file must stop it");
        assert!(err.to_string().contains("README.md"), "{err}");

        // The check happens before any write, so a refusal leaves no
        // half-scaffolded project behind.
        assert!(!dir.path().join("blocks").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn init_writes_a_complete_runnable_project() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), "demo").unwrap();
        for (path, _) in files("demo") {
            assert!(dir.path().join(&path).exists(), "missing {path}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join("tests/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the harness must be executable");
        }
    }

    #[test]
    fn a_name_that_would_not_parse_as_a_spec_identifier_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // It lands in `spec <name> = { ... }`, so a space or a quote would
        // produce a scaffold that cannot parse — better to refuse now than
        // to hand someone a broken project.
        for bad in ["has space", "quote\"", ""] {
            assert!(run(dir.path(), bad).is_err(), "accepted `{bad}`");
        }
    }
}
