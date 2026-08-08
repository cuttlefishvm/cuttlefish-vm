//! `cuttlefish build`, driven through the real binary.
//!
//! `cuttlefish build` packages a `Checked` pipeline into a linear
//! `.cfbundle` node array (`crates/cuttlefish-host/src/bundle.rs`), which
//! only knows how to encode a strict chain. A spec whose graph fans in (or
//! loops, or branches) must be refused up front with a clear message rather
//! than silently truncated into a wrong bundle — this is exactly the guard
//! added to `build_cmd` in `crates/cuttlefish/src/main.rs`, gated on
//! `cuttlefish_core::graph::is_simple_chain`.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

const CUTTLEFISH: &str = env!("CARGO_BIN_EXE_cuttlefish");

/// A spec whose `nodes` graph fans in: `c` reads both `a` and `b` through a
/// `Record` input (`{ x = a.out; y = b.out; }`). Individually, no node here
/// has more than one direct predecessor edge into a `FromNode` slot — the
/// fan-in is only visible as a `Record` input, which is exactly the shape
/// `is_simple_chain` rejects and `cuttlefish build`'s bundle format cannot
/// encode.
const FAN_IN_SPEC: &str = r#"
spec fan_in_spec = {
  description = "A spec whose graph fans in, used to test cuttlefish build's refusal.";
  model = Path "./models/stub.gguf";
  data_policy = Any;
  capabilities = [ ];
  nodes = {
    a = { block = "blocks/a.wasm" };
    b = { block = "blocks/b.wasm" };
    c = { block = "blocks/c.wasm"; in = { x = a.out; y = b.out; }; };
  };
}
"#;

fn run_build(spec_path: &Path) -> std::process::Output {
    Command::new(CUTTLEFISH)
        .args(["build"])
        .arg(spec_path)
        .output()
        .expect("the cuttlefish binary under test must be spawnable")
}

/// `cuttlefish build` must refuse a fan-in graph outright, with a message
/// naming the actual reason (fan-in/loop/branches, not encodable into a
/// linear bundle) rather than failing later with some unrelated error (a
/// missing wasm file, a catalog-resolution failure, etc.) that would happen
/// to also exit non-zero. The block paths in `FAN_IN_SPEC` don't even point
/// at real wasm — the refusal must happen before resolution is ever
/// attempted, since the graph shape is knowable from the parsed spec alone.
#[test]
fn build_refuses_a_fan_in_graph_before_touching_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("fan_in.cuttlefish");
    std::fs::write(&spec_path, FAN_IN_SPEC).unwrap();

    let out = run_build(&spec_path);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "build must fail on a fan-in graph: stdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("isn't a simple linear chain"),
        "the refusal must name the real reason (fan-in/loop/branches), not some \
         downstream resolution failure: {stderr}"
    );
    assert!(
        !stderr.contains("resolving the pipeline"),
        "the refusal must happen before block resolution is ever attempted: {stderr}"
    );

    // No bundle should have been written for a spec that was refused.
    assert!(!spec_path.with_extension("cfbundle").exists());
}

/// A minimal, valid `.rhai` script with a signature header — enough for
/// `catalog add`/`resolve_and_load`/`check` to accept it as a `Script`-kind
/// node; the interpreter itself is never instantiated during `check` (a
/// `Script` node's signature comes from its header comment, not wasm
/// introspection), so this test needs no real interpreter wasm to exist.
const SCRIPT_TEXT: &str = "//! signature: {n: json} -> {n: json}\ninput\n";

/// `cuttlefish build` must refuse a spec containing a `Script` node with a
/// clear error, rather than silently embedding a redundant copy of the
/// shared interpreter and dropping the actual script — `bundle::build`
/// has no field to carry script text at all, so doing so would either
/// panic or corrupt the bundle.
#[test]
fn build_refuses_a_spec_with_a_script_node() {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("echo.rhai");
    std::fs::write(&script_path, SCRIPT_TEXT).unwrap();

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let add = Command::new(CUTTLEFISH)
        .args(["catalog", "add", "echo@1"])
        .arg(&script_path)
        .env("CUTTLEFISH_HOME", &home)
        .output()
        .expect("the cuttlefish binary under test must be spawnable");
    assert!(
        add.status.success(),
        "cataloging the fixture script failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let spec_path = dir.path().join("script_node.cuttlefish");
    std::fs::write(
        &spec_path,
        r#"
spec script_node_spec = {
  description = "A spec with a Script node, used to test cuttlefish build's refusal.";
  model = Path "./models/stub.gguf";
  data_policy = Any;
  capabilities = [ ];
  nodes = {
    echo = { block = "echo@1" };
  };
}
"#,
    )
    .unwrap();

    let out = Command::new(CUTTLEFISH)
        .args(["build"])
        .arg(&spec_path)
        .env("CUTTLEFISH_HOME", &home)
        .output()
        .expect("the cuttlefish binary under test must be spawnable");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "build must refuse a Script node: stdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("echo") && stderr.to_lowercase().contains("script"),
        "the refusal must name the node and mention it's a script: {stderr}"
    );
    assert!(!spec_path.with_extension("cfbundle").exists());
}

#[test]
fn build_s_script_refusal_names_the_node_key_not_the_block_name() {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("echo.rhai");
    std::fs::write(&script_path, SCRIPT_TEXT).unwrap();

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let add = Command::new(CUTTLEFISH)
        .args(["catalog", "add", "echo@1"])
        .arg(&script_path)
        .env("CUTTLEFISH_HOME", &home)
        .output()
        .expect("the cuttlefish binary under test must be spawnable");
    assert!(
        add.status.success(),
        "cataloging the fixture script failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    // The node key ("sum") deliberately differs from the block it points at
    // ("echo@1") — the refusal must name the former, since that's what the
    // spec's own author wrote and would look for.
    let spec_path = dir.path().join("renamed_node.cuttlefish");
    std::fs::write(
        &spec_path,
        r#"
spec renamed_node_spec = {
  description = "A Script node whose key differs from its block name.";
  model = Path "./models/stub.gguf";
  data_policy = Any;
  capabilities = [ ];
  nodes = {
    sum = { block = "echo@1" };
  };
}
"#,
    )
    .unwrap();

    let out = Command::new(CUTTLEFISH)
        .args(["build"])
        .arg(&spec_path)
        .env("CUTTLEFISH_HOME", &home)
        .output()
        .expect("the cuttlefish binary under test must be spawnable");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "build must refuse a Script node");
    assert!(
        stderr.contains("`sum`"),
        "the refusal must name the node key `sum`, not the block name `echo`: {stderr}"
    );
}
