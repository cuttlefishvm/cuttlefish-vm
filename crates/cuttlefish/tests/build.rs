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
