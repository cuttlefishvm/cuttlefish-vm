# Cuttlefish Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Comment the *why*, in the code.** This project makes a number of non-obvious choices — a host-driven reactor instead of blocking host calls, handle-based bulk data instead of passing contents, a descriptor struct instead of packing a pointer pair into an `i64`. Each exists to dodge a specific failure that is invisible from reading the code that resulted. When you implement a step, carry its rationale comment across verbatim and add one wherever you make a judgment call the next reader could plausibly "simplify" away. Do not comment what a line does; comment the constraint that made it look like that. These comments are the project's memory — the design spec will drift, the code will not.

**Goal:** Build the thinnest end-to-end path — a `.cuttlefish` spec file configures a wasm job that the daemon runs under the reactor host ABI and streams a result back over a unix socket — with a stub inference backend instead of llama.cpp.

**Architecture:** A Cargo workspace of four crates. `cuttlefish-abi` holds the wire types shared by host and guest. `cuttlefish-sdk` is the guest-side library a proc-block author writes against; it hides the reactor state machine behind ordinary Rust and emits the raw wasm exports. `cuttlefish-host` is the wasmtime driver that calls those exports, executes the commands the guest returns, and enforces capabilities. `cuttlefishd` is the daemon binary wrapping the host in an HTTP-over-UDS API, plus `cuttlefish`, the CLI client. Spec parsing in this slice is deliberately minimal: a single-block pipeline, `Path` model refs, local block paths, no registry.

**Bulk data never enters guest memory.** A block does not receive file contents; it receives a *handle* and a length, then pulls bounded windows with `Slice`. The host seeks and reads each window straight off disk, so neither side ever holds a whole corpus. This keeps guest memory flat no matter how large the input grows, which is what lets the slice stay on 32-bit wasm without a 4 GiB input ceiling hanging over every block author. See "The 4 GiB question" below.

**Tech Stack:** Rust (workspace), wasmtime (host), `wasm32-unknown-unknown` (guest target), axum routing served over a hand-rolled hyper accept loop on a `UnixListener` (daemon API), tokio (async runtime + `CancellationToken`), serde/serde_json (wire format), a hand-written scanner for the spec subset.

**Spec:** `docs/superpowers/specs/2026-07-30-cuttlefish-design.md`

**Explicitly out of scope for this slice** (each is later-plan material, do not build): real llama.cpp inference, the type unifier and `.cfi` interface files, block registry, model pool with memory budgets and eviction, the agent-harness skills plugin, `Network` capability, DAG-level loops, and the whole `cuttlefish build` compile-and-link step — in this slice the daemon takes an already-built `.wasm` path as an argument, and `cuttlefish inspect`/`graph`/`push` do not exist.

**Environment rule:** every build/test command in this plan runs through `nix develop --command ...`. The toolchain is pinned by `flake.nix`; a bare `cargo` picks up whatever Homebrew or rustup has on `PATH`.

**The 4 GiB question (why not `wasm64` now):** a wasm32 guest tops out at 4 GiB of linear memory, and the obvious worry is a job whose input exceeds it. Two independent levers relieve that: raise the ceiling (`wasm64-unknown-unknown`), or stop putting bulk data in guest memory at all. This slice takes the second — `Open`/`Slice` handles, above — because it keeps guest memory flat rather than merely larger, and because it works on the stable toolchain today.

`wasm64` stays open as a later, additive choice, and two decisions here exist to keep it cheap:

1. **No pointer packing.** The guest returns a pointer to a `Desc { ptr, len }` struct rather than packing both into one `i64`. That packing trick only works while pointers are 32 bits; abandoning it now means the export signatures don't change when a 64-bit guest arrives.
2. **The host reads pointer width from the module, not from an assumption.** `Abi::detect` inspects `MemoryType::is_64()` and every pointer read goes through it. This slice implements only the 32-bit path and rejects 64-bit modules with a clear error — but the branch point exists, so adding the other path is an addition rather than a refactor.

The remaining costs of wasm64, for whenever it's worth paying: `wasm64-unknown-unknown` is Tier 3 with no prebuilt std, so guest builds need nightly plus `-Z build-std`; and 64-bit memories give up wasmtime's guard-page bounds-check elision, costing perhaps 10–20% on guest execution. That second cost matters unusually little here — the guest orchestrates while the host does inference, so the guest is not the hot loop. Note also that a single wasmtime engine with `wasm_memory64(true)` still runs 32-bit modules, so the eventual choice can be per-block rather than project-wide.

**Why `wasm32-unknown-unknown` and not `wasm32-wasip1`:** a Rust cdylib built for wasip1 links wasi-libc, so even the panic path imports `wasi_snapshot_preview1::fd_write` and `proc_exit`. Those imports must be satisfied at instantiation or `Linker::instantiate` fails with `unknown import`. Guest blocks in this slice do all their IO through host commands and need no WASI at all, so the unknown-unknown target keeps the host's `Store<()>`/`Linker<()>` genuinely empty. When a later plan needs WASI in guests, add `wasmtime-wasi`, switch the store type to `WasiP1Ctx`, call `add_to_linker_sync`, and invoke the module's `_initialize` export before `cf_alloc`.

---

### Task 1: Nix devShell gains the wasm target

The guest crates compile to `wasm32-unknown-unknown`. Stock `pkgs.rustc` in nixpkgs does not ship that target's std, so the flake needs `rust-overlay` to select a toolchain with it.

**Files:**
- Modify: `flake.nix`

- [ ] **Step 1: Add rust-overlay input and a toolchain with the wasm target**

Replace the `inputs` block and the `pkgs` binding in `flake.nix`:

```nix
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        # Guest proc-blocks compile to wasm32-unknown-unknown; the host is
        # native. One toolchain pinned here so both come from the same rustc.
        #
        # On wasm64: guests are capped at 4 GiB of linear memory here, which is
        # deliberate rather than accidental. Bulk data never enters guest memory
        # (blocks pull bounded windows through Open/Slice handles), so the cap
        # binds on nothing we actually do. If a future block genuinely needs a
        # >4 GiB resident working set, switch THAT block to
        # wasm64-unknown-unknown rather than the whole project: one wasmtime
        # engine with wasm_memory64(true) runs 32- and 64-bit modules side by
        # side, and cuttlefish-host already detects width via MemoryType::is_64.
        # The cost is real, so weigh it: wasm64 is Tier 3 with no prebuilt std,
        # so it needs
        #   pkgs.rust-bin.nightly.latest.default.override {
        #     targets = [ "wasm32-unknown-unknown" ];
        #     extensions = [ "rust-src" ];   # required by -Z build-std
        #   }
        # plus `-Z build-std=std,panic_abort` on the guest build, and 64-bit
        # memories lose wasmtime's guard-page bounds-check elision.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };
      in
```

- [ ] **Step 2: Swap the individual rust packages for the toolchain**

In `devShells.default.buildInputs`, replace the `pkgs.rustc`, `pkgs.cargo`, and `pkgs.wasm-pack` entries with `rustToolchain`. Keep `pkgs.maturin` and `pkgs.libiconv`. Leave the Python entries untouched.

- [ ] **Step 3: Verify the shell provides both targets**

Run: `nix develop --command bash -c "rustc --print target-list | grep -x wasm32-unknown-unknown && cargo --version"`
Expected: prints `wasm32-unknown-unknown` then a cargo version line.

- [ ] **Step 4: Commit**

```bash
git add flake.nix flake.lock
git commit -m "build: pin rust toolchain with wasm32-unknown-unknown target"
```

---

### Task 2: Cargo workspace skeleton

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/cuttlefish-abi/Cargo.toml`, `crates/cuttlefish-abi/src/lib.rs`
- Create: `.gitignore`

- [ ] **Step 1: Write the workspace manifest**

`Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/cuttlefish",        # facade + `cuttlefish` binary; the published crate
    "crates/cuttlefish-abi",
    "crates/cuttlefish-core",
    "crates/cuttlefish-sdk",
    "crates/cuttlefish-host",
    "crates/cuttlefishd",
    "blocks/echo-summarize",
]

# Members are listed explicitly rather than via the inherited `crates/*` glob,
# because `blocks/` sits outside `crates/` and a glob would silently miss it.

[workspace.package]
version = "0.1.0"
edition = "2021"
# Dual-licensed to match the existing published crate; do not diverge per-crate.
license = "MIT OR Apache-2.0"
repository = "https://github.com/cuttlefishvm/cuttlefish-vm.git"
homepage = "https://cuttlefishvm.github.io/"
# wasmtime 27 needs a far newer compiler than the 1.62.1 the placeholder crate
# declared. Raise this deliberately, not by accident.
rust-version = "1.79"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net", "io-util"] }
tokio-util = "0.7"
wasmtime = "27"
axum = "0.7"
```

- [ ] **Step 2: Write `.gitignore`**

```
/target
/.venv
result
```

- [ ] **Step 3: Write the abi crate manifest**

`crates/cuttlefish-abi/Cargo.toml`:

```toml
[package]
name = "cuttlefish-abi"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
```

- [ ] **Step 4: Write the shared wire types**

`crates/cuttlefish-abi/src/lib.rs`. These are the types crossing the wasm boundary as JSON, and the envelope crossing the HTTP boundary. Host and guest both depend on this crate, which is what keeps them from drifting.

```rust
//! Wire types shared by the wasm host and guest proc-blocks.
//!
//! Everything crosses the wasm boundary as JSON. That is slower than a packed
//! binary layout, but it keeps the ABI inspectable and the SDK simple; revisit
//! only if profiling says to.

use serde::{Deserialize, Serialize};

/// Opaque, job-scoped reference to something the host holds open on the
/// guest's behalf. Job-scoped is the security property: a handle from one job
/// is meaningless in another, so handles need no capability check of their own
/// — the check happened once, at `Open`.
pub type Handle = u32;

/// What a guest asks the host to do, returned from `init`/`step`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Infer { prompt: String, max_tokens: u32 },
    /// Ask the host to open a file. Capability-checked. Returns a handle and a
    /// byte length — deliberately *not* the contents, so guest memory stays
    /// flat regardless of file size.
    Open { path: String },
    /// Pull one bounded window of an open file into guest memory. The guest
    /// chooses the window size, so it decides its own memory ceiling.
    Slice { handle: Handle, offset: u64, len: u64 },
    Emit { progress: serde_json::Value },
    Done { result: serde_json::Value },
    Fail { code: String, message: String },
}

/// What the host feeds back into `step` after executing a command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    InferDone { text: String, tokens_out: u32 },
    Opened { handle: Handle, len: u64 },
    /// `next_offset` is where the returned text actually ended, which is not
    /// always `offset + len`: the host truncates the window down to a UTF-8
    /// character boundary. A guest that walks a file must resume from this
    /// value rather than advancing by its requested length, or it will corrupt
    /// multi-byte characters at every window seam.
    Sliced { text: String, next_offset: u64 },
    Emitted,
}

/// Guest's verdict on each streamed token, returned from `on_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAction {
    Continue,
    Stop,
}

impl TokenAction {
    pub fn from_i32(v: i32) -> Self {
        if v == 0 { Self::Continue } else { Self::Stop }
    }
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Continue => 0,
            Self::Stop => 1,
        }
    }
}

/// Terminal state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub duration_ms: u64,
    pub model: String,
}

/// The fixed, spec-independent envelope handed back to the calling agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

/// Error codes the daemon emits. Kept as constants rather than an enum so the
/// set can grow without a breaking change to clients matching on strings.
pub mod error_codes {
    pub const MODEL_LOAD_FAILED: &str = "model_load_failed";
    pub const CAPABILITY_DENIED: &str = "capability_denied";
    pub const SCHEMA_VALIDATION_FAILED: &str = "schema_validation_failed";
    pub const WASM_TRAP: &str = "wasm_trap";
    pub const TIMEOUT: &str = "timeout";
    pub const CANCELLED: &str = "cancelled";
}
```

- [ ] **Step 5: Write a round-trip test**

`crates/cuttlefish-abi/src/lib.rs`, appended:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trips_through_json() {
        let cmd = Command::Infer { prompt: "hi".into(), max_tokens: 32 };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"infer","prompt":"hi","max_tokens":32}"#);
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn event_round_trips_through_json() {
        let ev = Event::InferDone { text: "yo".into(), tokens_out: 2 };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn slice_command_round_trips_through_json() {
        let cmd = Command::Slice { handle: 7, offset: 4096, len: 1024 };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn envelope_omits_absent_optional_fields() {
        let env = Envelope {
            status: JobStatus::Completed,
            result: Some(serde_json::json!({"summary": "s"})),
            error: None,
            usage: Usage::default(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("error"), "absent error must not serialize: {json}");
    }
}
```

- [ ] **Step 6: Create stub crates for every other workspace member**

Cargo refuses to build a workspace whose members don't exist, so create all of them now rather than leaving the build knowingly broken between tasks. Each stub gets a `Cargo.toml` with just the package stanza:

```toml
[package]
name = "<crate-name>"
version.workspace = true
edition.workspace = true
```

Crate names and stub contents — note the block's package name is **not** its directory name, and both binary crates need a real `fn main() {}` body or the build fails with E0601:

| Path | `name` | Stub file |
|---|---|---|
| `crates/cuttlefish-core` | `cuttlefish-core` | empty `src/lib.rs` |
| `crates/cuttlefish-sdk` | `cuttlefish-sdk` | empty `src/lib.rs` |
| `crates/cuttlefish-host` | `cuttlefish-host` | empty `src/lib.rs` |
| `crates/cuttlefishd` | `cuttlefishd` | `src/main.rs` with `fn main() {}` |
| `blocks/echo-summarize` | `cf-block-echo-summarize` | empty `src/lib.rs` |

`crates/cuttlefish` already exists from the merged repo — do **not** recreate it. It keeps its published identity (name `cuttlefish`, the crates.io slug) and gains the CLI in Task 8; leave its `Cargo.toml` alone until then apart from switching `version`/`edition`/`license` to `.workspace = true`.

- [ ] **Step 7: Run the tests**

Run: `nix develop --command cargo test -p cuttlefish-abi`
Expected: 4 tests pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore crates blocks
git commit -m "feat(abi): add wire types shared by wasm host and guest"
```

---

### Task 3: Guest SDK — raw wasm exports

The guest is a state machine the host drives. This task builds the raw export layer: memory allocation, JSON in/out, and the three exports. Guest state lives in a thread-local rather than being serialized in and out, which is safe because the host creates one instance per job and never shares it.

**Files:**
- Create: `crates/cuttlefish-sdk/Cargo.toml`, `crates/cuttlefish-sdk/src/lib.rs`

- [ ] **Step 1: Write the manifest**

`crates/cuttlefish-sdk/Cargo.toml`:

```toml
[package]
name = "cuttlefish-sdk"
version.workspace = true
edition.workspace = true

[dependencies]
cuttlefish-abi = { path = "../cuttlefish-abi" }
serde = { workspace = true }
serde_json = { workspace = true }
```

- [ ] **Step 2: Write the result descriptor**

`crates/cuttlefish-sdk/src/lib.rs`:

```rust
//! Guest-side library for writing cuttlefish proc-blocks.

/// How a guest hands a (pointer, length) pair back to the host.
///
/// The obvious alternative — pack both into the single `i64` a wasm export can
/// return — works only while pointers are 32 bits. Returning a pointer to this
/// struct instead costs one extra memory read per call and keeps every export
/// signature identical under a 64-bit guest, where `usize` simply widens. Do
/// not "simplify" this back into bit-packing; that is the thing it exists to
/// avoid. See "The 4 GiB question" in the plan.
#[repr(C)]
pub struct Desc {
    pub ptr: usize,
    pub len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_layout_is_two_pointer_sized_fields() {
        // The host reads exactly this many bytes at the returned address, so
        // the layout is load-bearing, not incidental.
        assert_eq!(
            std::mem::size_of::<Desc>(),
            2 * std::mem::size_of::<usize>()
        );
        assert_eq!(std::mem::align_of::<Desc>(), std::mem::align_of::<usize>());
    }
}
```

- [ ] **Step 3: Run it**

Run: `nix develop --command cargo test -p cuttlefish-sdk`
Expected: PASS.

- [ ] **Step 4: Add the Block trait and export macro**

Append to `crates/cuttlefish-sdk/src/lib.rs`:

```rust
use cuttlefish_abi::{Command, Event, TokenAction};

/// What a proc-block author implements. The host calls `start` once, then
/// `step` after each command it executes, until the block returns
/// `Command::Done` or `Command::Fail`.
pub trait Block: Default {
    /// First command, derived from the job's input JSON.
    fn start(&mut self, input: serde_json::Value) -> Command;

    /// Next command, given the result of the previous one.
    fn step(&mut self, event: Event) -> Command;

    /// Called per streamed token during an `Infer`. Default keeps going.
    fn on_token(&mut self, _token: &str) -> TokenAction {
        TokenAction::Continue
    }
}

/// Guest-side allocation the host writes into. Leaks deliberately: the host
/// owns the buffer's lifetime and the instance is torn down after one job.
#[doc(hidden)]
pub fn __alloc(len: usize) -> usize {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr() as usize;
    std::mem::forget(buf);
    ptr
}

#[doc(hidden)]
pub unsafe fn __read_json<T: serde::de::DeserializeOwned>(ptr: usize, len: usize) -> T {
    let slice = std::slice::from_raw_parts(ptr as *const u8, len);
    serde_json::from_slice(slice).expect("host sent malformed JSON")
}

/// Serialize `value` into guest memory and return the address of a `Desc`
/// describing it. Both the payload and the descriptor are leaked — the host
/// reads them immediately after the call returns, and the instance is torn
/// down after one job, so there is nothing to reclaim.
#[doc(hidden)]
pub fn __write_json<T: serde::Serialize>(value: &T) -> usize {
    let bytes = serde_json::to_vec(value).expect("guest produced unserializable value");
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8 as usize;
    Box::into_raw(Box::new(Desc { ptr, len })) as usize
}

/// Emit the wasm exports for a `Block` implementation.
///
/// The block's state is a thread-local because the host instantiates one
/// module per job and never shares it across jobs.
#[macro_export]
macro_rules! export_block {
    ($ty:ty) => {
        thread_local! {
            static __CF_STATE: ::std::cell::RefCell<$ty> =
                ::std::cell::RefCell::new(<$ty as ::core::default::Default>::default());
        }

        // Every export takes and returns `usize`, never a packed `i64`. On
        // wasm32 that lowers to i32 params/results; on a 64-bit guest the same
        // source compiles to i64 with no signature change here.
        #[no_mangle]
        pub extern "C" fn cf_alloc(len: usize) -> usize {
            $crate::__alloc(len)
        }

        #[no_mangle]
        pub extern "C" fn cf_init(ptr: usize, len: usize) -> usize {
            let input: ::serde_json::Value = unsafe { $crate::__read_json(ptr, len) };
            let cmd = __CF_STATE.with(|s| $crate::Block::start(&mut *s.borrow_mut(), input));
            $crate::__write_json(&cmd)
        }

        #[no_mangle]
        pub extern "C" fn cf_step(ptr: usize, len: usize) -> usize {
            let event: ::cuttlefish_abi::Event = unsafe { $crate::__read_json(ptr, len) };
            let cmd = __CF_STATE.with(|s| $crate::Block::step(&mut *s.borrow_mut(), event));
            $crate::__write_json(&cmd)
        }

        #[no_mangle]
        pub extern "C" fn cf_on_token(ptr: usize, len: usize) -> i32 {
            let token: ::std::string::String = unsafe {
                let slice = ::std::slice::from_raw_parts(ptr as *const u8, len);
                ::std::string::String::from_utf8_lossy(slice).into_owned()
            };
            __CF_STATE
                .with(|s| $crate::Block::on_token(&mut *s.borrow_mut(), &token))
                .as_i32()
        }
    };
}
```

- [ ] **Step 5: Run the tests again**

Run: `nix develop --command cargo test -p cuttlefish-sdk`
Expected: PASS (still 1 test; the macro is exercised by Task 4's block).

- [ ] **Step 6: Commit**

```bash
git add crates/cuttlefish-sdk Cargo.lock
git commit -m "feat(sdk): add Block trait and wasm export macro"
```

---

### Task 4: The example proc-block

A block that reads a file, asks the model to summarize it, and returns the summary. Small enough to reason about, but it exercises every command variant the slice supports.

**Files:**
- Create: `blocks/echo-summarize/Cargo.toml`, `blocks/echo-summarize/src/lib.rs`

- [ ] **Step 1: Write the manifest**

`blocks/echo-summarize/Cargo.toml`:

```toml
[package]
name = "cf-block-echo-summarize"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib"]

[dependencies]
cuttlefish-sdk = { path = "../../crates/cuttlefish-sdk" }
cuttlefish-abi = { path = "../../crates/cuttlefish-abi" }
serde_json = { workspace = true }
```

- [ ] **Step 2: Write the block**

`blocks/echo-summarize/src/lib.rs`:

```rust
use cuttlefish_abi::{Command, Event, TokenAction};
use cuttlefish_sdk::{export_block, Block};

/// Summarizes one file: opens it, pulls a bounded window, summarizes that.
///
/// Note what this block never does: hold the file. It sees a handle and a
/// length, and it decides how much to bring into its own memory. A block that
/// wanted the whole file would loop on `Slice` from `next_offset` — its memory
/// ceiling would still be `WINDOW`, not the file size.
///
/// The `stop_after_first` input flag exists so the host's early-stop path has
/// a block that actually exercises it; a real block would decide from content.
#[derive(Default)]
struct EchoSummarize {
    path: String,
    stop_after_first: bool,
    seen_tokens: u32,
}

/// How much of a file this block is willing to hold at once.
const WINDOW: u64 = 64 * 1024;

impl Block for EchoSummarize {
    fn start(&mut self, input: serde_json::Value) -> Command {
        self.stop_after_first = input
            .get("stop_after_first")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        match input.get("path").and_then(|v| v.as_str()) {
            Some(path) => {
                self.path = path.to_string();
                Command::Open { path: path.to_string() }
            }
            None => Command::Fail {
                code: cuttlefish_abi::error_codes::SCHEMA_VALIDATION_FAILED.into(),
                message: "input must have a string `path` field".into(),
            },
        }
    }

    fn step(&mut self, event: Event) -> Command {
        match event {
            Event::Opened { handle, len } => Command::Slice {
                handle,
                offset: 0,
                len: len.min(WINDOW),
            },
            Event::Sliced { text, .. } => Command::Infer {
                prompt: format!("Summarize the following:\n\n{text}"),
                max_tokens: 128,
            },
            Event::InferDone { text, .. } => Command::Done {
                result: serde_json::json!({ "path": self.path, "summary": text }),
            },
            Event::Emitted => Command::Fail {
                code: "unexpected_event".into(),
                message: "block never emits progress".into(),
            },
        }
    }

    fn on_token(&mut self, _token: &str) -> TokenAction {
        self.seen_tokens += 1;
        if self.stop_after_first && self.seen_tokens >= 1 {
            TokenAction::Stop
        } else {
            TokenAction::Continue
        }
    }
}

export_block!(EchoSummarize);
```

- [ ] **Step 3: Build it to wasm**

Run: `nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown`
Expected: builds; `target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm` exists.

- [ ] **Step 4: Commit**

```bash
git add blocks/echo-summarize Cargo.lock
git commit -m "feat(blocks): add echo-summarize example proc-block"
```

---

### Task 5: Host driver — instantiate and run the reactor loop

The core of the slice. The host owns the loop: call `cf_init`, get a command, execute it, call `cf_step` with the resulting event, repeat until `Done`/`Fail`. Inference goes through a trait so this task needs no real model.

**Files:**
- Create: `crates/cuttlefish-host/Cargo.toml`
- Create: `crates/cuttlefish-host/src/lib.rs`, `src/infer.rs`, `src/caps.rs`, `src/handles.rs`, `src/runner.rs`
- Test: `crates/cuttlefish-host/tests/caps.rs`, `crates/cuttlefish-host/tests/handles.rs`, `crates/cuttlefish-host/tests/runner.rs`

- [ ] **Step 1: Write the manifest**

`crates/cuttlefish-host/Cargo.toml`:

```toml
[package]
name = "cuttlefish-host"
version.workspace = true
edition.workspace = true

[dependencies]
cuttlefish-abi = { path = "../cuttlefish-abi" }
wasmtime = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
async-trait = "0.1"
```

- [ ] **Step 2: Write the failing capability tests first**

Write `crates/cuttlefish-host/tests/caps.rs` (below) and the `[dev-dependencies]` stanza, then run `nix develop --command cargo test -p cuttlefish-host --test caps` and confirm it fails to compile because `caps` does not exist. Only then write the implementation.

`crates/cuttlefish-host/src/caps.rs`:

```rust
use std::path::{Path, PathBuf};

/// What a spec grants a job. v1 has exactly one capability kind.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    read_roots: Vec<PathBuf>,
}

impl Capabilities {
    pub fn new(read_roots: Vec<PathBuf>) -> Self {
        Self { read_roots }
    }

    /// Deny-by-default: a path is allowed only if it canonicalizes to something
    /// under a granted root. Canonicalizing both sides is what stops `../`
    /// traversal and symlink escapes from slipping past a string prefix check.
    pub fn allows_read(&self, path: &Path) -> bool {
        let Ok(target) = path.canonicalize() else {
            return false;
        };
        self.read_roots.iter().any(|root| {
            root.canonicalize()
                .map(|r| target.starts_with(r))
                .unwrap_or(false)
        })
    }
}
```

`crates/cuttlefish-host/tests/caps.rs`:

```rust
use cuttlefish_host::caps::Capabilities;
use std::fs;

#[test]
fn allows_read_under_granted_root() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ok.txt");
    fs::write(&file, "hi").unwrap();
    let caps = Capabilities::new(vec![dir.path().to_path_buf()]);
    assert!(caps.allows_read(&file));
}

#[test]
fn denies_read_outside_granted_root() {
    let granted = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let file = other.path().join("secret.txt");
    fs::write(&file, "nope").unwrap();
    let caps = Capabilities::new(vec![granted.path().to_path_buf()]);
    assert!(!caps.allows_read(&file));
}

#[test]
fn denies_traversal_out_of_granted_root() {
    let root = tempfile::tempdir().unwrap();
    let inner = root.path().join("inner");
    fs::create_dir(&inner).unwrap();
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "nope").unwrap();
    let caps = Capabilities::new(vec![inner.clone()]);
    assert!(!caps.allows_read(&inner.join("../outside.txt")));
}

#[test]
fn denies_when_no_capabilities_granted() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, "x").unwrap();
    assert!(!Capabilities::default().allows_read(&file));
}
```

Add to `crates/cuttlefish-host/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: Run the capability tests**

Run: `nix develop --command cargo test -p cuttlefish-host --test caps`
Expected: 4 pass.

- [ ] **Step 4: Write the file handle table**

`crates/cuttlefish-host/src/handles.rs`. This is the host side of Lever B: the guest gets a handle, the host keeps the file open and serves bounded windows straight off disk. Neither side ever holds the whole file.

```rust
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// One file the host holds open for a job.
struct OpenFile {
    file: File,
    len: u64,
}

/// Job-scoped table of open files.
///
/// Scoping matters for more than tidiness: because a table lives and dies with
/// one job, a handle from another job cannot name anything here. That is what
/// lets `Slice` skip a capability check — the check happened once, at `open`,
/// and a handle is unforgeable across jobs by construction.
#[derive(Default)]
pub struct Handles {
    next: u32,
    open: HashMap<u32, OpenFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum HandleError {
    #[error("no such handle: {0}")]
    BadHandle(u32),
    #[error("offset {offset} is past end of file ({len} bytes)")]
    OffsetPastEnd { offset: u64, len: u64 },
    #[error("window of {0} bytes is too small to hold one character")]
    WindowTooSmall(u64),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct Window {
    pub text: String,
    /// Where the returned text actually ended. Not necessarily
    /// `offset + requested_len` — see `slice`.
    pub next_offset: u64,
}

impl Handles {
    /// Open a file and return its handle and length. The caller is responsible
    /// for having capability-checked `path` first.
    pub fn open(&mut self, path: &Path) -> Result<(u32, u64), HandleError> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let handle = self.next;
        self.next += 1;
        self.open.insert(handle, OpenFile { file, len });
        Ok((handle, len))
    }

    /// Read one window, truncated to a UTF-8 character boundary.
    ///
    /// The truncation is the subtle part. A caller walking a file picks window
    /// sizes with no idea where characters begin, so a naive read splits a
    /// multi-byte character at nearly every seam and produces mojibake. Instead
    /// the window is cut back to the last complete character and `next_offset`
    /// reports where that landed — so a caller that resumes from `next_offset`
    /// (rather than advancing by its own requested length) never sees a split.
    pub fn slice(&mut self, handle: u32, offset: u64, len: u64) -> Result<Window, HandleError> {
        let f = self.open.get_mut(&handle).ok_or(HandleError::BadHandle(handle))?;
        if offset > f.len {
            return Err(HandleError::OffsetPastEnd { offset, len: f.len });
        }

        let want = len.min(f.len - offset) as usize;
        let mut buf = vec![0u8; want];
        f.file.seek(SeekFrom::Start(offset))?;
        f.file.read_exact(&mut buf)?;

        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) => e.valid_up_to(),
        };
        // A window landing entirely inside one character would otherwise return
        // empty forever and hang a caller that loops until it reaches the end.
        if valid == 0 && !buf.is_empty() {
            return Err(HandleError::WindowTooSmall(len));
        }
        buf.truncate(valid);

        Ok(Window {
            text: String::from_utf8(buf).expect("truncated at a validated boundary"),
            next_offset: offset + valid as u64,
        })
    }
}
```

`crates/cuttlefish-host/tests/handles.rs`:

```rust
use cuttlefish_host::handles::Handles;
use std::io::Write;

fn temp_with(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    (dir, path)
}

#[test]
fn open_reports_length_without_reading_contents() {
    let (_d, path) = temp_with("hello world");
    let mut h = Handles::default();
    let (handle, len) = h.open(&path).unwrap();
    assert_eq!(len, 11);
    assert_eq!(handle, 0);
}

#[test]
fn slice_returns_the_requested_window() {
    let (_d, path) = temp_with("hello world");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();
    let w = h.slice(handle, 0, 5).unwrap();
    assert_eq!(w.text, "hello");
    assert_eq!(w.next_offset, 5);
}

#[test]
fn slice_is_clamped_to_end_of_file() {
    let (_d, path) = temp_with("hi");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();
    let w = h.slice(handle, 0, 4096).unwrap();
    assert_eq!(w.text, "hi");
    assert_eq!(w.next_offset, 2);
}

#[test]
fn slice_truncates_to_a_character_boundary() {
    // "é" is two bytes, so a 3-byte window over "aéb" would split it.
    let (_d, path) = temp_with("aéb");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();
    let w = h.slice(handle, 0, 2).unwrap();
    assert_eq!(w.text, "a", "must not return half a character");
    assert_eq!(w.next_offset, 1, "caller resumes before the split character");

    // Resuming from next_offset yields the character intact.
    let w2 = h.slice(handle, w.next_offset, 2).unwrap();
    assert_eq!(w2.text, "é");
}

#[test]
fn window_too_small_for_one_character_is_an_error_not_an_empty_read() {
    let (_d, path) = temp_with("é");
    let mut h = Handles::default();
    let (handle, _) = h.open(&path).unwrap();
    assert!(h.slice(handle, 0, 1).is_err(), "must not loop forever returning nothing");
}

#[test]
fn unknown_handle_is_rejected() {
    let mut h = Handles::default();
    assert!(h.slice(99, 0, 10).is_err());
}
```

- [ ] **Step 5: Run the handle tests**

Run: `nix develop --command cargo test -p cuttlefish-host --test handles`
Expected: 6 pass.

- [ ] **Step 6: Write the inference backend trait and stub**

`crates/cuttlefish-host/src/infer.rs`:

```rust
use async_trait::async_trait;

pub struct InferResult {
    pub text: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

/// Everything that can serve an `Infer` command. The slice ships only
/// `StubBackend`; llama.cpp arrives behind this same trait in a later plan.
#[async_trait]
pub trait InferBackend: Send + Sync {
    /// `on_token` is invoked per token; returning `false` stops generation
    /// early, which is how the guest's `cf_on_token` verdict is honored.
    ///
    /// The `for<'t>` is load-bearing: `#[async_trait]` rewrites elided
    /// lifetimes into named ones, which would make this closure non-generic
    /// over the token's lifetime and leave implementations unable to pass a
    /// local `&str` to it (E0597).
    async fn infer(
        &self,
        prompt: &str,
        max_tokens: u32,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult>;

    fn model_name(&self) -> String;
}

/// Deterministic fake: echoes a fixed reply word by word so streaming,
/// early-stop, and token accounting are all testable without a model.
///
/// It yields between tokens. That matters: without an await point the whole
/// loop would run inside a single poll, every token would land in the channel
/// at once, and the host could never interleave a guest `Stop` verdict — the
/// early-stop path would look correct while being dead code. A real llama.cpp
/// backend awaits naturally; the stub has to do it deliberately.
pub struct StubBackend {
    pub reply: String,
}

impl Default for StubBackend {
    fn default() -> Self {
        Self { reply: "a stub summary".into() }
    }
}

#[async_trait]
impl InferBackend for StubBackend {
    async fn infer(
        &self,
        prompt: &str,
        max_tokens: u32,
        on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        let mut out = String::new();
        let mut tokens_out = 0u32;
        for word in self.reply.split_whitespace().take(max_tokens as usize) {
            let piece = if out.is_empty() { word.to_string() } else { format!(" {word}") };
            tokens_out += 1;
            let keep_going = on_token(&piece);
            out.push_str(&piece);
            if !keep_going {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(InferResult {
            text: out,
            tokens_in: prompt.split_whitespace().count() as u32,
            tokens_out,
        })
    }

    fn model_name(&self) -> String {
        "stub".into()
    }
}
```

- [ ] **Step 7: Write the runner**

`crates/cuttlefish-host/src/runner.rs`. This is the reactor loop from the spec. Note the two things it deliberately does *not* do: it never blocks the guest on the host, and it never lets the guest cancel itself — cancellation is the host declining to step.

```rust
use crate::caps::Capabilities;
use crate::handles::Handles;
use crate::infer::InferBackend;
use cuttlefish_abi::{error_codes, Command, Envelope, Event, JobError, JobStatus, Usage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// Progress the daemon forwards to a job's SSE stream.
#[derive(Debug, Clone)]
pub enum JobEvent {
    Token(String),
    Progress(serde_json::Value),
}

pub struct JobSpec {
    pub module_bytes: Vec<u8>,
    pub input: serde_json::Value,
    pub caps: Capabilities,
}

/// Pointer width of a guest module, read from the module itself rather than
/// assumed.
///
/// This slice implements only `W32` and rejects 64-bit modules outright. The
/// enum exists anyway so that supporting wasm64 later is an added arm plus a
/// second set of `TypedFunc` signatures — not a hunt through the file for
/// every place a pointer was assumed to be 32 bits. See "The 4 GiB question".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    W32,
    W64,
}

impl Abi {
    /// Size of one pointer-sized field, and therefore half a `Desc`.
    fn ptr_size(self) -> usize {
        match self {
            Abi::W32 => 4,
            Abi::W64 => 8,
        }
    }
}

struct Guest {
    store: Store<()>,
    memory: Memory,
    abi: Abi,
    alloc: TypedFunc<u32, u32>,
    init: TypedFunc<(u32, u32), u32>,
    step: TypedFunc<(u32, u32), u32>,
    on_token: Option<TypedFunc<(u32, u32), i32>>,
}

impl Guest {
    fn new(engine: &Engine, module_bytes: &[u8]) -> anyhow::Result<Self> {
        let module = Module::new(engine, module_bytes)?;
        let linker: Linker<()> = Linker::new(engine);
        let mut store = Store::new(engine, ());
        let instance: Instance = linker.instantiate(&mut store, &module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow::anyhow!("guest exports no memory"))?;

        // Width comes from the module's own memory type. A 64-bit guest would
        // export cf_init as (i64, i64) -> i64, so the get_typed_func calls
        // below would fail with a confusing signature-mismatch error; failing
        // here instead says what is actually wrong.
        let abi = if memory.ty(&store).is_64() { Abi::W64 } else { Abi::W32 };
        if abi == Abi::W64 {
            anyhow::bail!(
                "guest uses 64-bit memory; only 32-bit guests are supported in this build"
            );
        }

        Ok(Self {
            alloc: instance.get_typed_func(&mut store, "cf_alloc")?,
            init: instance.get_typed_func(&mut store, "cf_init")?,
            step: instance.get_typed_func(&mut store, "cf_step")?,
            on_token: instance.get_typed_func(&mut store, "cf_on_token").ok(),
            memory,
            abi,
            store,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<(u32, u32)> {
        let len = bytes.len() as u32;
        let ptr = self.alloc.call(&mut self.store, len)?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok((ptr, len))
    }

    /// Read the `Desc { ptr, len }` the guest returned, then the payload it
    /// points at. Two reads instead of unpacking one integer — the cost of
    /// keeping these signatures identical across pointer widths.
    fn read_desc(&mut self, desc_ptr: u32) -> anyhow::Result<Vec<u8>> {
        let w = self.abi.ptr_size();
        let mut desc = vec![0u8; 2 * w];
        self.memory.read(&mut self.store, desc_ptr as usize, &mut desc)?;

        let field = |bytes: &[u8]| -> u64 {
            match w {
                4 => u32::from_le_bytes(bytes.try_into().unwrap()) as u64,
                _ => u64::from_le_bytes(bytes.try_into().unwrap()),
            }
        };
        let ptr = field(&desc[..w]) as usize;
        let len = field(&desc[w..]) as usize;

        let mut buf = vec![0u8; len];
        self.memory.read(&mut self.store, ptr, &mut buf)?;
        Ok(buf)
    }

    fn call_init(&mut self, input: &serde_json::Value) -> anyhow::Result<Command> {
        let bytes = serde_json::to_vec(input)?;
        let (ptr, len) = self.write(&bytes)?;
        let desc = self.init.call(&mut self.store, (ptr, len))?;
        Ok(serde_json::from_slice(&self.read_desc(desc)?)?)
    }

    fn call_step(&mut self, event: &Event) -> anyhow::Result<Command> {
        let bytes = serde_json::to_vec(event)?;
        let (ptr, len) = self.write(&bytes)?;
        let desc = self.step.call(&mut self.store, (ptr, len))?;
        Ok(serde_json::from_slice(&self.read_desc(desc)?)?)
    }

    /// Returns whether generation should continue.
    fn call_on_token(&mut self, token: &str) -> anyhow::Result<bool> {
        // Cloned, not moved: wasmtime's TypedFunc is not Copy, so `let
        // Some(f) = self.on_token` behind `&mut self` is E0507. Cloning also
        // ends the borrow of `self` before `write` needs it mutably.
        let Some(f) = self.on_token.clone() else { return Ok(true) };
        let (ptr, len) = self.write(token.as_bytes())?;
        Ok(f.call(&mut self.store, (ptr, len))? == 0)
    }
}

fn fail(code: &str, message: impl Into<String>, usage: Usage) -> Envelope {
    Envelope {
        status: JobStatus::Failed,
        result: None,
        error: Some(JobError { code: code.into(), message: message.into() }),
        usage,
    }
}

/// Drive one job to completion. The guest is stepped by this loop and never
/// calls back into the host, which is what makes cancellation trivial: stop
/// stepping.
pub async fn run_job(
    engine: Arc<Engine>,
    backend: Arc<dyn InferBackend>,
    job: JobSpec,
    events: mpsc::Sender<JobEvent>,
    cancel: CancellationToken,
) -> Envelope {
    let started = Instant::now();
    let mut usage = Usage { model: backend.model_name(), ..Usage::default() };
    // Dropped with this function, closing every file the job opened. Job-scoped
    // lifetime is what makes handles unforgeable across jobs.
    let mut handles = Handles::default();

    let mut guest = match Guest::new(&engine, &job.module_bytes) {
        Ok(g) => g,
        Err(e) => return fail(error_codes::WASM_TRAP, e.to_string(), usage),
    };

    let mut command = match guest.call_init(&job.input) {
        Ok(c) => c,
        Err(e) => return fail(error_codes::WASM_TRAP, e.to_string(), usage),
    };

    loop {
        if cancel.is_cancelled() {
            usage.duration_ms = started.elapsed().as_millis() as u64;
            return Envelope {
                status: JobStatus::Cancelled,
                result: None,
                error: Some(JobError {
                    code: error_codes::CANCELLED.into(),
                    message: "job cancelled".into(),
                }),
                usage,
            };
        }

        let event = match command {
            Command::Done { result } => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return Envelope { status: JobStatus::Completed, result: Some(result), error: None, usage };
            }
            Command::Fail { code, message } => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return fail(&code, message, usage);
            }
            Command::Emit { progress } => {
                let _ = events.send(JobEvent::Progress(progress)).await;
                Event::Emitted
            }
            // The capability check lives here, at Open, and nowhere else. Slice
            // takes a handle rather than a path, and handles are job-scoped, so
            // there is no second place a path could enter.
            Command::Open { path } => {
                let p = std::path::PathBuf::from(&path);
                if !job.caps.allows_read(&p) {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(
                        error_codes::CAPABILITY_DENIED,
                        format!("read not permitted: {path}"),
                        usage,
                    );
                }
                match handles.open(&p) {
                    Ok((handle, len)) => Event::Opened { handle, len },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
                    }
                }
            }
            Command::Slice { handle, offset, len } => {
                match handles.slice(handle, offset, len) {
                    Ok(w) => Event::Sliced { text: w.text, next_offset: w.next_offset },
                    Err(e) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(error_codes::CAPABILITY_DENIED, e.to_string(), usage);
                    }
                }
            }
            Command::Infer { prompt, max_tokens } => {
                // Tokens must reach the guest *while* generation runs — the
                // guest's Stop verdict is what ends it early. The wasmtime
                // Store is !Sync so the guest can't be touched from inside the
                // backend's callback; a channel carries tokens out and a shared
                // flag carries the verdict back in, without sharing the Store.
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                let stop = Arc::new(AtomicBool::new(false));
                let sink_stop = stop.clone();
                let mut sink = move |t: &str| {
                    tx.send(t.to_string()).is_ok() && !sink_stop.load(Ordering::Relaxed)
                };

                let mut trap: Option<String> = None;
                let outcome = {
                    let infer = backend.infer(&prompt, max_tokens, &mut sink);
                    tokio::pin!(infer);
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break None,
                            Some(tok) = rx.recv() => {
                                let _ = events.send(JobEvent::Token(tok.clone())).await;
                                match guest.call_on_token(&tok) {
                                    Ok(true) => {}
                                    Ok(false) => stop.store(true, Ordering::Relaxed),
                                    Err(e) => {
                                        trap = Some(e.to_string());
                                        break None;
                                    }
                                }
                            }
                            r = &mut infer => break Some(r),
                        }
                    }
                };

                if let Some(message) = trap {
                    usage.duration_ms = started.elapsed().as_millis() as u64;
                    return fail(error_codes::WASM_TRAP, message, usage);
                }

                // Tokens generated in the same poll as the final one are still
                // queued; forward them so the stream is complete.
                while let Ok(tok) = rx.try_recv() {
                    let _ = events.send(JobEvent::Token(tok)).await;
                }

                match outcome {
                    None => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return Envelope {
                            status: JobStatus::Cancelled,
                            result: None,
                            error: Some(JobError {
                                code: error_codes::CANCELLED.into(),
                                message: "cancelled during inference".into(),
                            }),
                            usage,
                        };
                    }
                    Some(Err(e)) => {
                        usage.duration_ms = started.elapsed().as_millis() as u64;
                        return fail(error_codes::MODEL_LOAD_FAILED, e.to_string(), usage);
                    }
                    Some(Ok(r)) => {
                        usage.tokens_in += r.tokens_in;
                        usage.tokens_out += r.tokens_out;
                        Event::InferDone { text: r.text, tokens_out: r.tokens_out }
                    }
                }
            }
        };

        command = match guest.call_step(&event) {
            Ok(c) => c,
            Err(e) => {
                usage.duration_ms = started.elapsed().as_millis() as u64;
                return fail(error_codes::WASM_TRAP, e.to_string(), usage);
            }
        };
    }
}
```

`crates/cuttlefish-host/src/lib.rs`:

```rust
pub mod caps;
pub mod handles;
pub mod infer;
pub mod runner;
```

- [ ] **Step 8: Write the end-to-end runner test**

`crates/cuttlefish-host/tests/runner.rs`. It builds the example block first so the test exercises real wasm, not a mock.

```rust
use cuttlefish_abi::JobStatus;
use cuttlefish_host::{caps::Capabilities, infer::StubBackend, runner::{run_job, JobSpec}};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

/// Builds the example block and returns its wasm bytes. Building here rather
/// than checking in a binary keeps the fixture honest as the SDK changes.
///
/// The guest is always built in debug: `cargo test --release` would otherwise
/// look in a `release/` directory this never populates.
pub fn example_block() -> Vec<u8> {
    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "cf-block-echo-summarize",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .status()
        .expect("cargo build failed to start");
    assert!(status.success(), "building the example block failed");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join("target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm"))
        .expect("built wasm artifact not found")
}

#[tokio::test]
async fn runs_a_job_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("doc.txt");
    std::fs::write(&file, "some document text").unwrap();

    let (tx, mut rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            module_bytes: example_block(),
            input: serde_json::json!({ "path": file.to_str().unwrap() }),
            caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        },
        tx,
        CancellationToken::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let result = envelope.result.expect("completed job must carry a result");
    assert_eq!(result["summary"], "a stub summary");
    assert!(envelope.usage.tokens_out > 0, "usage must be accounted");

    let mut streamed = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        streamed.push(ev);
    }
    assert!(!streamed.is_empty(), "tokens must reach the event stream");
}

#[tokio::test]
async fn denies_read_outside_capability() {
    let granted = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let secret = other.path().join("secret.txt");
    std::fs::write(&secret, "proprietary").unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            module_bytes: example_block(),
            input: serde_json::json!({ "path": secret.to_str().unwrap() }),
            caps: Capabilities::new(vec![granted.path().to_path_buf()]),
        },
        tx,
        CancellationToken::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Failed);
    assert_eq!(envelope.error.unwrap().code, cuttlefish_abi::error_codes::CAPABILITY_DENIED);
    assert!(envelope.result.is_none(), "failed job must not carry a partial result");
}

#[tokio::test]
async fn guest_stop_verdict_truncates_generation() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("doc.txt");
    std::fs::write(&file, "text").unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            module_bytes: example_block(),
            input: serde_json::json!({
                "path": file.to_str().unwrap(),
                "stop_after_first": true,
            }),
            caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        },
        tx,
        CancellationToken::new(),
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Completed);
    let summary = envelope.result.unwrap()["summary"].as_str().unwrap().to_string();
    // The stub's full reply is three tokens. Asserting "fewer than three"
    // rather than an exact string keeps the test honest about the one-token
    // lag between the guest's verdict and the backend observing it.
    assert!(
        envelope.usage.tokens_out < 3,
        "stop verdict must cut generation short, got {} tokens ({summary:?})",
        envelope.usage.tokens_out
    );
    assert_ne!(summary, "a stub summary", "stop verdict must truncate output");
}

#[tokio::test]
async fn cancelled_before_start_yields_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("doc.txt");
    std::fs::write(&file, "text").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let (tx, _rx) = mpsc::channel(64);
    let envelope = run_job(
        Arc::new(Engine::default()),
        Arc::new(StubBackend::default()),
        JobSpec {
            module_bytes: example_block(),
            input: serde_json::json!({ "path": file.to_str().unwrap() }),
            caps: Capabilities::new(vec![dir.path().to_path_buf()]),
        },
        tx,
        cancel,
    )
    .await;

    assert_eq!(envelope.status, JobStatus::Cancelled);
}
```

- [ ] **Step 9: Run the tests**

Run: `nix develop --command cargo test -p cuttlefish-host`
Expected: 14 pass (4 caps + 6 handles + 4 runner).

- [ ] **Step 10: Commit**

```bash
git add crates/cuttlefish-host Cargo.lock
git commit -m "feat(host): add reactor-model job runner with capability enforcement"
```

---

### Task 6: Minimal spec parser

The full typed DSL with `.cfi` unification is a later plan. This slice parses the subset the vertical slice needs: name, description, `Path` model, capabilities, and a single-block pipeline. Anything richer is a parse error, not a silent partial parse.

**Files:**
- Create: `crates/cuttlefish-core/src/lib.rs`, `crates/cuttlefish-core/src/spec.rs`
- Modify: `crates/cuttlefish-core/Cargo.toml` (stub from Task 2)
- Test: `crates/cuttlefish-core/tests/parse.rs`
- Create: `examples/summarize.cuttlefish`

- [ ] **Step 1: Write the example spec file**

`examples/summarize.cuttlefish`:

```
spec summarize_docs = {
  description = "Use when the agent needs a summary of a local file and content must not leave the machine.";
  model = Path "../models/stub.gguf";
  data_policy = Local_only;
  capabilities = [ Read "./docs" ];
  block = "../blocks/echo-summarize";
}
```

Paths here are relative to the spec file's own directory, not to the daemon's working directory — `Read "./docs"` means `examples/docs`. (`block` is parsed but unused in this slice; the daemon takes the built `.wasm` as an argument, since `cuttlefish build` is out of scope.)

- [ ] **Step 2: Write the failing parse tests**

`crates/cuttlefish-core/tests/parse.rs`:

```rust
use cuttlefish_core::spec::{parse_spec, DataPolicy, ModelRef};

const SAMPLE: &str = r#"
spec summarize_docs = {
  description = "Use when the agent needs a summary of a local file.";
  model = Path "./models/stub.gguf";
  data_policy = Local_only;
  capabilities = [ Read "./examples/docs" ];
  block = "./blocks/echo-summarize";
}
"#;

#[test]
fn parses_all_fields() {
    let spec = parse_spec(SAMPLE).expect("sample must parse");
    assert_eq!(spec.name, "summarize_docs");
    assert!(spec.description.starts_with("Use when"));
    assert_eq!(spec.model, ModelRef::Path("./models/stub.gguf".into()));
    assert_eq!(spec.data_policy, DataPolicy::LocalOnly);
    assert_eq!(spec.read_roots, vec![std::path::PathBuf::from("./examples/docs")]);
    assert_eq!(spec.block, std::path::PathBuf::from("./blocks/echo-summarize"));
}

#[test]
fn rejects_missing_required_field() {
    let missing_block = SAMPLE.replace(r#"  block = "./blocks/echo-summarize";"#, "");
    let err = parse_spec(&missing_block).unwrap_err();
    assert!(err.to_string().contains("block"), "error must name the missing field: {err}");
}

#[test]
fn rejects_unknown_field_rather_than_ignoring_it() {
    let extra = SAMPLE.replace("  block =", "  frobnicate = 3;\n  block =");
    let err = parse_spec(&extra).unwrap_err();
    assert!(err.to_string().contains("frobnicate"), "error must name the unknown field: {err}");
}

#[test]
fn rejects_hf_model_ref_in_this_slice() {
    let hf = SAMPLE.replace(r#"Path "./models/stub.gguf""#, r#"Hf "org/model#q4""#);
    let err = parse_spec(&hf).unwrap_err();
    assert!(err.to_string().contains("Hf"), "unsupported model kind must be named: {err}");
}
```

- [ ] **Step 3: Run them to confirm they fail**

Run: `nix develop --command cargo test -p cuttlefish-core`
Expected: FAIL — crate/function does not exist.

- [ ] **Step 4: Implement the parser**

`crates/cuttlefish-core/Cargo.toml`:

```toml
[package]
name = "cuttlefish-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { workspace = true }
thiserror = { workspace = true }
```

`crates/cuttlefish-core/src/spec.rs`. A hand-written scanner, not chumsky: the grammar in this slice is a flat key/value block, and reaching for a parser-combinator library before the DSL grows loops and expressions would be building the wrong abstraction. Chumsky arrives with the real typed DSL.

```rust
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum ModelRef {
    Path(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPolicy {
    LocalOnly,
    Any,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub name: String,
    pub description: String,
    pub model: ModelRef,
    pub data_policy: DataPolicy,
    pub read_roots: Vec<PathBuf>,
    pub block: PathBuf,
}

#[derive(Debug, Error)]
pub enum SpecError {
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("unknown field `{0}`")]
    UnknownField(String),
    #[error("malformed spec: {0}")]
    Malformed(String),
    #[error("unsupported model kind `{0}` (this build supports only `Path`)")]
    UnsupportedModel(String),
}

fn quoted(value: &str, field: &'static str) -> Result<String, SpecError> {
    let t = value.trim();
    t.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .map(|s| s.to_string())
        .ok_or_else(|| SpecError::Malformed(format!("field `{field}` must be a quoted string")))
}

pub fn parse_spec(src: &str) -> Result<Spec, SpecError> {
    let open = src
        .find('{')
        .ok_or_else(|| SpecError::Malformed("expected `{`".into()))?;
    let close = src
        .rfind('}')
        .ok_or_else(|| SpecError::Malformed("expected `}`".into()))?;

    let header = src[..open].trim();
    let name = header
        .strip_prefix("spec")
        .and_then(|h| h.split('=').next())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| SpecError::Malformed("expected `spec <name> = {`".into()))?;

    let (mut description, mut model, mut data_policy, mut read_roots, mut block) =
        (None, None, None, None, None);

    for stmt in src[open + 1..close].split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        let (key, value) = stmt
            .split_once('=')
            .ok_or_else(|| SpecError::Malformed(format!("expected `key = value` in `{stmt}`")))?;
        let value = value.trim();
        match key.trim() {
            "description" => description = Some(quoted(value, "description")?),
            "block" => block = Some(PathBuf::from(quoted(value, "block")?)),
            "model" => {
                let (kind, rest) = value
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| SpecError::Malformed("model needs a kind and a value".into()))?;
                match kind {
                    "Path" => model = Some(ModelRef::Path(quoted(rest, "model")?)),
                    other => return Err(SpecError::UnsupportedModel(other.to_string())),
                }
            }
            "data_policy" => {
                data_policy = Some(match value {
                    "Local_only" => DataPolicy::LocalOnly,
                    "Any" => DataPolicy::Any,
                    other => {
                        return Err(SpecError::Malformed(format!("unknown data_policy `{other}`")))
                    }
                })
            }
            "capabilities" => {
                let inner = value
                    .strip_prefix('[')
                    .and_then(|v| v.strip_suffix(']'))
                    .ok_or_else(|| SpecError::Malformed("capabilities must be a `[...]` list".into()))?;
                let mut roots = Vec::new();
                for entry in inner.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                    let rest = entry
                        .strip_prefix("Read")
                        .ok_or_else(|| SpecError::Malformed(format!("unknown capability `{entry}`")))?;
                    roots.push(PathBuf::from(quoted(rest, "capabilities")?));
                }
                read_roots = Some(roots);
            }
            other => return Err(SpecError::UnknownField(other.to_string())),
        }
    }

    Ok(Spec {
        name,
        description: description.ok_or(SpecError::MissingField("description"))?,
        model: model.ok_or(SpecError::MissingField("model"))?,
        data_policy: data_policy.ok_or(SpecError::MissingField("data_policy"))?,
        read_roots: read_roots.ok_or(SpecError::MissingField("capabilities"))?,
        block: block.ok_or(SpecError::MissingField("block"))?,
    })
}
```

`crates/cuttlefish-core/src/lib.rs`:

```rust
pub mod spec;
```

`crates/cuttlefish-core` is already a workspace member (created as a stub in Task 2); this task fills it in.

- [ ] **Step 5: Run the tests**

Run: `nix develop --command cargo test -p cuttlefish-core`
Expected: 4 pass.

- [ ] **Step 6: Commit**

```bash
git add crates/cuttlefish-core examples Cargo.toml Cargo.lock
git commit -m "feat(core): parse the minimal .cuttlefish spec subset"
```

---

### Task 7: Daemon — job store and HTTP API over a unix socket

**Files:**
- Create: `crates/cuttlefishd/src/lib.rs`, `src/main.rs`, `src/state.rs`, `src/api.rs`, `src/serve.rs`
- Modify: `crates/cuttlefishd/Cargo.toml` (stub from Task 2)
- Test: `crates/cuttlefishd/tests/api.rs`

- [ ] **Step 1: Write the manifest**

```toml
[package]
name = "cuttlefishd"
version.workspace = true
edition.workspace = true

[dependencies]
cuttlefish-abi = { path = "../cuttlefish-abi" }
cuttlefish-core = { path = "../cuttlefish-core" }
cuttlefish-host = { path = "../cuttlefish-host" }
axum = { workspace = true }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "sync", "time", "net", "io-util", "signal", "fs"] }
tokio-util = { workspace = true }
# `sync` is not a default feature and is what gates BroadcastStream.
tokio-stream = { version = "0.1", features = ["sync"] }
futures-util = "0.3"
wasmtime = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
hyper = { version = "1", features = ["server", "http1"] }
# `service` gates TowerToHyperService, `tokio` gates TokioIo.
hyper-util = { version = "0.1", features = ["tokio", "service"] }
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
tempfile = "3"
# `unix_socket` on ClientBuilder is cfg(unix) and needs no extra feature,
# but it is recent — do not pin below 0.12.20.
reqwest = { version = "0.12.20", default-features = false, features = ["json", "stream"] }
```

- [ ] **Step 2: Write the job store**

`crates/cuttlefishd/src/state.rs`. Finished envelopes are retained here rather than existing only on the SSE stream — that is what makes a dropped connection non-fatal.

Events are *also* retained, in a per-job replay log. This is not optional polish: `tokio::sync::broadcast` has no replay for late subscribers — `send` returns early when the receiver count is zero and does not even write to the ring buffer, and `subscribe()` starts at the current tail. A client that POSTs a job and then makes a second round trip to attach to `/events` will reliably miss every event of a fast job. The spec's `Last-Event-ID` replay requirement lands on this same log.

```rust
use cuttlefish_abi::{Envelope, JobStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Job {
    pub id: String,
    pub status: JobStatus,
    pub envelope: Option<Envelope>,
    pub cancel: CancellationToken,
    /// Live events for already-attached subscribers.
    pub events: broadcast::Sender<String>,
    /// Everything ever published, so a late subscriber can catch up.
    /// Unbounded, which is fine at this slice's scale (a job emits tens of
    /// events); a real cap belongs here once jobs get long.
    pub log: Vec<String>,
}

#[derive(Clone, Default)]
pub struct JobStore {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
}

impl JobStore {
    pub async fn insert(&self, job: Job) {
        self.jobs.lock().await.insert(job.id.clone(), job);
    }

    pub async fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().await.get(id).cloned()
    }

    /// Append to the replay log and fan out to live subscribers under one lock,
    /// so an event can never be both missed by a subscriber and absent from the
    /// log. `send` failing just means nobody is attached yet — the log covers it.
    pub async fn publish(&self, id: &str, payload: String) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            job.log.push(payload.clone());
            let _ = job.events.send(payload);
        }
    }

    /// Snapshot the backlog and attach a live receiver under the *same* lock
    /// guard. Splitting these would reopen the gap this exists to close.
    pub async fn subscribe(&self, id: &str) -> Option<(Vec<String>, broadcast::Receiver<String>)> {
        let jobs = self.jobs.lock().await;
        let job = jobs.get(id)?;
        Some((job.log.clone(), job.events.subscribe()))
    }

    pub async fn finish(&self, id: &str, envelope: Envelope) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            job.status = envelope.status;
            job.envelope = Some(envelope);
        }
    }

    pub async fn cancel(&self, id: &str) -> bool {
        match self.jobs.lock().await.get(id) {
            Some(job) => {
                job.cancel.cancel();
                true
            }
            None => false,
        }
    }
}
```

- [ ] **Step 3: Write the API**

`crates/cuttlefishd/src/api.rs`:

```rust
use crate::state::{Job, JobStore};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::{Event as SseEvent, Sse}, IntoResponse},
    routing::{delete, get, post},
    Json, Router,
};
use cuttlefish_abi::{Envelope, JobStatus, Usage};
use cuttlefish_core::spec::Spec;
use cuttlefish_host::{caps::Capabilities, infer::InferBackend, runner::{run_job, JobEvent, JobSpec}};
use futures_util::{stream::Stream, StreamExt};
use serde::Deserialize;
use std::{convert::Infallible, sync::Arc};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use wasmtime::Engine;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub backend: Arc<dyn InferBackend>,
    pub jobs: JobStore,
    pub spec: Arc<Spec>,
    pub module_bytes: Arc<Vec<u8>>,
}

#[derive(Deserialize)]
pub struct SubmitJob {
    pub spec: String,
    pub input: serde_json::Value,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/jobs", post(submit))
        .route("/jobs/:id", get(get_job).delete(cancel_job))
        .route("/jobs/:id/events", get(job_events))
        .route("/specs", get(list_specs))
        .with_state(state)
}

async fn list_specs(State(st): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!([{
        "name": st.spec.name,
        "description": st.spec.description,
    }]))
}

async fn submit(State(st): State<AppState>, Json(req): Json<SubmitJob>) -> impl IntoResponse {
    if req.spec != st.spec.name {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("unknown spec `{}`", req.spec)})),
        )
            .into_response();
    }

    let id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let (events_tx, _) = broadcast::channel(256);

    st.jobs
        .insert(Job {
            id: id.clone(),
            status: JobStatus::Running,
            envelope: None,
            cancel: cancel.clone(),
            events: events_tx.clone(),
            log: Vec::new(),
        })
        .await;

    let job_spec = JobSpec {
        module_bytes: (*st.module_bytes).clone(),
        input: req.input,
        caps: Capabilities::new(st.spec.read_roots.clone()),
    };

    // Every event goes through `publish` so it lands in the replay log as well
    // as the live broadcast — a client that attaches after the job finished
    // still sees the whole stream.
    let (tx, mut rx) = mpsc::channel::<JobEvent>(256);
    let (fwd_store, fwd_id) = (st.jobs.clone(), id.clone());
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let payload = match ev {
                JobEvent::Token(t) => serde_json::json!({"type": "token", "text": t}),
                JobEvent::Progress(p) => serde_json::json!({"type": "progress", "progress": p}),
            };
            fwd_store.publish(&fwd_id, payload.to_string()).await;
        }
    });

    let (engine, backend, store, jid) = (st.engine.clone(), st.backend.clone(), st.jobs.clone(), id.clone());
    tokio::spawn(async move {
        let envelope = run_job(engine, backend, job_spec, tx, cancel).await;
        store
            .publish(
                &jid,
                serde_json::json!({"type": "result", "envelope": envelope}).to_string(),
            )
            .await;
        store.finish(&jid, envelope).await;
    });

    (StatusCode::ACCEPTED, Json(serde_json::json!({"job_id": id}))).into_response()
}

async fn get_job(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.jobs.get(&id).await {
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such job"}))).into_response(),
        Some(job) => Json(serde_json::json!({
            "job_id": job.id,
            "status": job.status,
            "envelope": job.envelope.unwrap_or(Envelope {
                status: job.status,
                result: None,
                error: None,
                usage: Usage::default(),
            }),
        }))
        .into_response(),
    }
}

async fn cancel_job(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if st.jobs.cancel(&id).await {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn job_events(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    let (backlog, rx) = st.jobs.subscribe(&id).await.ok_or(StatusCode::NOT_FOUND)?;
    let replay = futures_util::stream::iter(
        backlog.into_iter().map(|m| Ok(SseEvent::default().data(m))),
    );
    let live = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|msg| futures_util::future::ready(msg.ok().map(|m| Ok(SseEvent::default().data(m)))));
    Ok(Sse::new(replay.chain(live)))
}
```

- [ ] **Step 4: Write `main.rs` binding a unix socket**

`axum::serve` in 0.7 takes a concrete `tokio::net::TcpListener` — the generic listener trait only arrives in axum 0.8 — so a unix socket needs a hand-rolled accept loop. That is why the manifest carries `hyper` and `hyper-util`. (Bumping to axum 0.8 instead is not a free swap: its path syntax changes, so every `/jobs/:id` route would become `/jobs/{id}`.)

`crates/cuttlefishd/src/serve.rs`:

```rust
use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use std::path::Path;
use tokio::net::UnixListener;

/// Serve an axum Router over a unix socket.
pub async fn serve_unix(app: Router, sock_path: &Path) -> anyhow::Result<()> {
    // A stale socket file from a previous run would make bind() fail with
    // EADDRINUSE even though nothing is listening.
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    eprintln!("cuttlefishd listening on {}", sock_path.display());

    loop {
        let (stream, _addr) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = TowerToHyperService::new(app);
            if let Err(e) = http1::Builder::new()
                // SSE responses stay open indefinitely; without this the
                // connection is closed out from under a streaming client.
                .keep_alive(true)
                .serve_connection(io, service)
                .await
            {
                eprintln!("connection error: {e}");
            }
        });
    }
}
```

`crates/cuttlefishd/src/main.rs`:

```rust
use anyhow::Context;
use cuttlefish_host::infer::StubBackend;
use cuttlefishd::{api, serve, state};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let spec_path = PathBuf::from(
        args.next().context("usage: cuttlefishd <spec> <block.wasm> <socket>")?,
    );
    let wasm_path = args.next().context("usage: cuttlefishd <spec> <block.wasm> <socket>")?;
    let sock_path = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/cuttlefish.sock".into()));

    let mut spec = cuttlefish_core::spec::parse_spec(&std::fs::read_to_string(&spec_path)?)?;

    // Capability roots are written relative to the spec file, not to wherever
    // the daemon happens to be started from. Resolving them here is what keeps
    // `Read "./examples/docs"` meaning the same thing regardless of cwd.
    let spec_dir = spec_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    spec.read_roots = spec.read_roots.iter().map(|r| spec_dir.join(r)).collect();

    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(StubBackend::default()),
        jobs: state::JobStore::default(),
        spec: Arc::new(spec),
        module_bytes: Arc::new(std::fs::read(&wasm_path)?),
    };

    serve::serve_unix(api::router(state), &sock_path).await
}
```

- [ ] **Step 5: Write the API integration test**

`crates/cuttlefishd/tests/api.rs`. Polling `GET /jobs/:id` rather than reading the SSE stream is deliberate: it proves results survive independently of the stream, which is the whole point of retaining envelopes.

`reqwest`'s `ClientBuilder::unix_socket` (cfg(unix), no extra feature) handles the UDS dial; the `http://localhost` in each URL is a placeholder authority the socket path overrides.

```rust
use cuttlefishd::{api, serve, state::JobStore};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// Same fixture as the host tests. Duplicated rather than shared because a
/// test-support crate for one helper is not worth a workspace member yet.
///
/// Assumes the default target directory; a custom `CARGO_TARGET_DIR` would
/// need reading here too.
fn example_block() -> Vec<u8> {
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "cf-block-echo-summarize", "--target", "wasm32-unknown-unknown"])
        .status()
        .expect("cargo build failed to start");
    assert!(status.success(), "building the example block failed");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join("target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm"))
        .expect("built wasm artifact not found")
}

struct Harness {
    client: reqwest::Client,
    _dir: tempfile::TempDir,
    doc: std::path::PathBuf,
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.txt");
    std::fs::write(&doc, "some document text").unwrap();
    let sock = dir.path().join("cf.sock");

    let spec = cuttlefish_core::spec::Spec {
        name: "summarize_docs".into(),
        description: "Use when a local file needs summarizing.".into(),
        model: cuttlefish_core::spec::ModelRef::Path("./stub.gguf".into()),
        data_policy: cuttlefish_core::spec::DataPolicy::LocalOnly,
        read_roots: vec![dir.path().to_path_buf()],
        block: "./blocks/echo-summarize".into(),
    };

    let state = api::AppState {
        engine: Arc::new(wasmtime::Engine::default()),
        backend: Arc::new(cuttlefish_host::infer::StubBackend::default()),
        jobs: JobStore::default(),
        spec: Arc::new(spec),
        module_bytes: Arc::new(example_block()),
    };

    let sock_for_server = sock.clone();
    tokio::spawn(async move {
        let _ = serve::serve_unix(api::router(state), &sock_for_server).await;
    });

    // Wait for the socket to appear rather than sleeping a fixed duration.
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Harness {
        // `as_path()`, not `&sock`: the sealed UnixSocketProvider trait covers
        // `&Path` and `PathBuf` but not `&PathBuf`, and no deref coercion
        // happens at an `impl Trait` parameter.
        client: reqwest::Client::builder().unix_socket(sock.as_path()).build().unwrap(),
        _dir: dir,
        doc,
    }
}

/// Poll until the job reaches a terminal status, or fail the test.
async fn await_terminal(h: &Harness, id: &str) -> serde_json::Value {
    for _ in 0..200 {
        let body: serde_json::Value = h
            .client
            .get(format!("http://localhost/jobs/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        match body["status"].as_str().unwrap() {
            "completed" | "failed" | "cancelled" => return body,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    panic!("job {id} never reached a terminal status");
}

#[tokio::test]
async fn submits_and_completes_a_job() {
    let h = start().await;
    let submit = h
        .client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({
            "spec": "summarize_docs",
            "input": { "path": h.doc.to_str().unwrap() }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(submit.status(), 202);

    let id = submit.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let body = await_terminal(&h, &id).await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["envelope"]["result"]["summary"], "a stub summary");
}

#[tokio::test]
async fn streams_tokens_over_sse() {
    let h = start().await;
    let id = h
        .client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({
            "spec": "summarize_docs",
            "input": { "path": h.doc.to_str().unwrap() }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Attaching here is a second round trip, by which time this stub job has
    // usually finished — so this only passes because the JobStore replays its
    // event log to late subscribers. The stream also never closes on its own
    // (the store holds a live sender), hence reading chunks until both markers
    // appear under one overall timeout rather than awaiting the whole body.
    // A smoke test that streaming works, not an SSE conformance test.
    let resp = h
        .client
        .get(format!("http://localhost/jobs/{id}/events"))
        .send()
        .await
        .unwrap();

    let body = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = resp.bytes_stream();
        let mut seen = String::new();
        while let Some(chunk) = stream.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if seen.contains(r#""type":"token""#) && seen.contains(r#""type":"result""#) {
                break;
            }
        }
        seen
    })
    .await
    .expect("timed out waiting for token and result events");

    assert!(body.contains(r#""type":"token""#), "no token events in stream: {body}");
    assert!(body.contains(r#""type":"result""#), "no result event in stream: {body}");
}

#[tokio::test]
async fn unknown_job_is_not_found() {
    let h = start().await;
    let resp = h
        .client
        .get("http://localhost/jobs/does-not-exist")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn unknown_spec_is_rejected() {
    let h = start().await;
    let resp = h
        .client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({ "spec": "nope", "input": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn cancel_accepts_a_live_job() {
    let h = start().await;
    let id = h
        .client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({
            "spec": "summarize_docs",
            "input": { "path": h.doc.to_str().unwrap() }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = h
        .client
        .delete(format!("http://localhost/jobs/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    // The job may already have completed — cancellation is best-effort, so the
    // assertion is that the endpoint accepts it, not that the job was caught.
    await_terminal(&h, &id).await;
}
```

For the test to reach `api`, `serve`, and `state`, the crate needs a library target alongside its binary. Add `crates/cuttlefishd/src/lib.rs`:

```rust
pub mod api;
pub mod serve;
pub mod state;
```

and change `main.rs` to use the library (`use cuttlefishd::{api, serve, state};`) rather than declaring the modules itself. Add to `Cargo.toml`:

```toml
[lib]
name = "cuttlefishd"
path = "src/lib.rs"

[[bin]]
name = "cuttlefishd"
path = "src/main.rs"
```

- [ ] **Step 6: Run the tests**

Run: `nix develop --command cargo test -p cuttlefishd`
Expected: 5 pass (submit/complete, SSE streaming, unknown job, unknown spec, cancel).

- [ ] **Step 7: Commit**

```bash
git add crates/cuttlefishd Cargo.lock
git commit -m "feat(daemon): serve job API over a unix socket with SSE and durable results"
```

---

### Task 8: CLI client, in the published `cuttlefish` crate

`crates/cuttlefish` is the crate already published to crates.io (v0.0.0 placeholder, name secured since 2022). It becomes the user-facing entry point: it re-exports the workspace pieces for docs, and ships the `cuttlefish` binary — so `cargo install cuttlefish` gives you the CLI and the published name matches the command name.

**Files:**
- Modify: `crates/cuttlefish/Cargo.toml`, `crates/cuttlefish/src/lib.rs`
- Create: `crates/cuttlefish/src/main.rs`

- [ ] **Step 1: Update the manifest**

Keep the existing package identity — `name`, `description`, `authors`, `homepage`, `repository`, `readme`, `categories`, and the `html_logo_url` in `lib.rs` are the crate's published face and must survive. Change only what the workspace now owns, plus the corrected description:

```toml
[package]
name = "cuttlefish"
description = "Native tooling for agents: a local wasm runtime that runs jobs against local models"
authors = ["Michael-F-Bryan <michaelfbryan@gmail.com>", "Kartik Thakore <kartik@thakore.ai>"]
categories = ["wasm"]
readme = "../../README.md"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
homepage.workspace = true
rust-version.workspace = true

[lib]
name = "cuttlefish"
path = "src/lib.rs"

[[bin]]
name = "cuttlefish"
path = "src/main.rs"

[dependencies]
cuttlefish-abi = { path = "../cuttlefish-abi" }
cuttlefish-core = { path = "../cuttlefish-core" }
clap = { version = "4", features = ["derive"] }
tokio = { workspace = true }
# Same UDS client as the daemon's tests; do not pin below 0.12.20.
reqwest = { version = "0.12.20", default-features = false, features = ["json"] }
serde_json = { workspace = true }
anyhow = { workspace = true }
```

- [ ] **Step 2: Keep the library as the documented facade**

`crates/cuttlefish/src/lib.rs` — preserve the existing `html_logo_url` doc attribute (it's what makes the published API docs carry the project logo) and re-export the workspace pieces so the crates.io docs have real surface instead of a placeholder:

```rust
//! Native tooling for agents: a local wasm runtime that runs jobs against
//! local models.
//!
//! This crate is the facade. The moving parts live in sibling crates and are
//! re-exported here so that `docs.rs/cuttlefish` documents the whole system
//! from one page.
//!
//! [cuttlefish]: https://cuttlefishvm.github.io/

#![doc(html_logo_url = "https://cuttlefishvm.github.io/assets/images/logo.png")]

pub use cuttlefish_abi as abi;
pub use cuttlefish_core as core;
```

- [ ] **Step 3: Write `cuttlefish run`**

`crates/cuttlefish/src/main.rs`. It POSTs the job, polls `GET /jobs/:id` until terminal, prints the envelope as JSON, and exits 0/1/2 for completed/failed/cancelled so shell callers and agents can branch without parsing stdout.

```rust
use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "cuttlefish", about = "Client for the cuttlefish daemon")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Submit a job and wait for its result.
    Run {
        #[arg(long, default_value = "/tmp/cuttlefish.sock")]
        socket: PathBuf,
        #[arg(long)]
        spec: String,
        /// Job input as a JSON object.
        #[arg(long)]
        input: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Cmd::Run { socket, spec, input } = Cli::parse().command;

    let input: serde_json::Value =
        serde_json::from_str(&input).context("--input must be valid JSON")?;
    // `as_path()`, not `&socket`: the sealed UnixSocketProvider trait covers
    // `&Path` and `PathBuf` but not `&PathBuf`.
    let client = reqwest::Client::builder()
        .unix_socket(socket.as_path())
        .build()
        .context("building the unix-socket client")?;

    // The authority in these URLs is ignored; the socket path decides where
    // the request actually goes.
    let submit = client
        .post("http://localhost/jobs")
        .json(&serde_json::json!({ "spec": spec, "input": input }))
        .send()
        .await
        .with_context(|| format!("connecting to daemon at {}", socket.display()))?;

    if !submit.status().is_success() {
        bail!("daemon rejected the job: {} {}", submit.status(), submit.text().await?);
    }

    let job_id = submit.json::<serde_json::Value>().await?["job_id"]
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
        if matches!(status, "completed" | "failed" | "cancelled") {
            println!("{}", serde_json::to_string_pretty(&body["envelope"])?);
            std::process::exit(match status {
                "completed" => 0,
                "failed" => 1,
                _ => 2,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
```

- [ ] **Step 4: Verify against a live daemon**

```bash
nix develop --command bash -c '
  set -e
  cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
  cargo build -p cuttlefishd -p cuttlefish-cli
  mkdir -p examples/docs && echo "hello world" > examples/docs/a.txt
  ./target/debug/cuttlefishd examples/summarize.cuttlefish \
    target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm /tmp/cf.sock &
  daemon=$!
  trap "kill $daemon" EXIT
  until [ -S /tmp/cf.sock ]; do sleep 0.1; done
  ./target/debug/cuttlefish run --socket /tmp/cf.sock --spec summarize_docs \
    --input "{\"path\": \"examples/docs/a.txt\"}"
'
```
Expected: a JSON envelope with `"status":"completed"` and `"summary":"a stub summary"`, and exit status 0.

- [ ] **Step 4: Commit**

```bash
git add crates/cuttlefish examples Cargo.lock
git commit -m "feat(cli): add cuttlefish run client to the published facade crate"
```

---

### Task 9: Green CI and slice wrap-up

The repository scaffolding — README, CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, the modernized CI workflow, and `codecov.yml` — already exists on `main`. This task makes it *true* rather than aspirational.

**Files:**
- Modify: `README.md`, `.github/workflows/ci.yml` (only if a job proves wrong)

- [ ] **Step 1: Run the full suite the way CI will**

```bash
nix develop --command cargo fmt --all --check
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command cargo test --workspace --locked
```
Expected: all pass. `--locked` matters — CI uses it, so a `Cargo.lock` that drifts from the manifests fails there and not here.

- [ ] **Step 2: Check coverage locally and see where it's thin**

```bash
nix develop --command cargo llvm-cov --workspace --locked
```

Read the per-file output rather than the headline number. The parts that matter are `caps.rs` and `handles.rs` — they are the sandbox boundary, and an uncovered branch there is an unenforced rule, not a missing test. If either has uncovered lines, add tests before moving on.

Note that `blocks/` will show as uncovered and that is expected: guest blocks execute inside wasm, where the host's instrumentation cannot reach. They are covered behaviourally through the host's tests, which is why `codecov.yml` ignores them.

- [ ] **Step 3: Verify the MSRV job would pass**

```bash
nix develop --command cargo check --workspace --all-targets --locked
```

If the workspace needs a newer compiler than the `rust-version = "1.79"` in `Cargo.toml`, raise that field *and* the pin in the `msrv` CI job together, and say why in the commit message.

- [ ] **Step 4: Verify every command in the README actually works**

Run each command block from the README verbatim in a clean shell, including the end-to-end example. Fix the README, not your memory of it, wherever they diverge. Then update its Status section to say what now runs.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: describe the working vertical slice"
```

---

## What this slice proves, and what comes next

Proven end to end once Task 9 is green: the reactor host ABI works over real wasm, capabilities are enforced deny-by-default with traversal-safe path checks, bulk data reaches blocks through bounded windows so guest memory never tracks input size, cancellation is a host-side decision needing no guest cooperation, results survive a dropped connection, and the spec file drives all of it.

Deliberately still missing, roughly in the order the follow-on plans should take them: real llama.cpp behind `InferBackend`, the typed DSL with `.cfi` unification and parametric block signatures, multi-block DAG pipelines, the model pool with per-job contexts and memory budgets, the block registry, and the agent-harness skills plugin.

Two decisions made here that later work should honour rather than quietly undo:

- **`wasm64` is a config change, not a rewrite.** No pointer packing anywhere, and `Abi::detect` reads width from the module. Adding 64-bit support means a second set of `TypedFunc` signatures and dropping the bail — not a hunt for assumed pointer sizes.
- **`crates/cuttlefish` is the published identity.** It carries the crates.io name (secured since 2022), the logo, and the dual MIT/Apache-2.0 licensing. Renaming or splitting it costs the published slug; the workspace grows around it instead.
