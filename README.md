# Cuttlefish VM

<p align="center">
  <a href="https://cuttlefishvm.github.io"><img src="https://cuttlefishvm.github.io/assets/images/logo.png" alt="Logo" height=170></a>
  <br />
  <br />
  <a href="https://github.com/cuttlefishvm/cuttlefish-vm/actions/workflows/ci.yml" target="_blank"><img alt="Continuous Integration" src="https://github.com/cuttlefishvm/cuttlefish-vm/actions/workflows/ci.yml/badge.svg?branch=main" /></a>
  <a href="https://codecov.io/gh/cuttlefishvm/cuttlefish-vm" target="_blank"><img alt="Coverage" src="https://codecov.io/gh/cuttlefishvm/cuttlefish-vm/branch/main/graph/badge.svg" /></a>
  <a href="https://crates.io/crates/cuttlefish" target="_blank"><img alt="crates.io" src="https://img.shields.io/crates/v/cuttlefish.svg" /></a>
  <a href="https://docs.rs/cuttlefish" target="_blank"><img alt="docs.rs" src="https://docs.rs/cuttlefish/badge.svg" /></a>
  <img alt="MSRV" src="https://img.shields.io/badge/rustc-1.79+-blue.svg" />
  <a href="#license" target="_blank"><img alt="License" src="https://img.shields.io/crates/l/cuttlefish.svg" /></a>
  <a href="https://discord.gg/KZGmKcayAT" target="_blank"><img alt="Discord" src="https://img.shields.io/discord/1012403879557738637" /></a>
</p>

([API Docs][api-docs])

Cuttlefish is native tooling for AI agents. A coding agent hands off a job — summarize this, classify that, extract these fields — and Cuttlefish runs it locally against a local model, returning a structured result.

Two things make that worth doing:

- **Your data stays on your machine.** For jobs marked local-only, the calling agent passes *paths*, not contents. Cuttlefish reads the files itself, so proprietary source and personal data never enter a frontier model's context at all.
- **You stop paying frontier prices for grunt work.** Bulk, repetitive, and mechanical subtasks don't need a trillion-parameter model. Offloading them keeps tokens and context for the work that does.

The tradeoff is honest and worth stating up front: you cannot run a frontier-class model on a laptop. Cuttlefish is for the large subset of agent work that doesn't need frontier reasoning — not a replacement for it.

## How it works

A job is described by a `.cuttlefish` spec: which model, what it's allowed to touch, and which processing block runs it. The block compiles to WebAssembly and runs sandboxed inside the `cuttlefishd` daemon, which serves inference from a local model and streams results back.

```
spec summarize_docs = {
  description = "Use when the agent needs a summary of a local file
                  and content must not leave the machine.";
  model = Path "../models/qwen2.5-7b-instruct-q4_k_m.gguf";
  data_policy = Local_only;
  capabilities = [ Read "./docs" ];
  block = "../blocks/echo-summarize";
}
```

Three design decisions shape everything else:

**The guest orchestrates; the host does the work.** A block is a state machine the daemon drives — it returns *commands* (`Infer`, `Open`, `Slice`, `Done`) and the host executes them. Nothing blocks waiting on a callback, so cancellation is simply the host declining to take the next step, and every inference iteration is observable and metered.

**Bulk data never enters guest memory.** Blocks receive a handle and a length, then pull bounded windows. Guest memory stays flat whether the input is a README or a corpus.

**Capabilities are deny-by-default and enforced twice.** A block gets no filesystem or network access unless its spec grants it. The grant is checked when the spec compiles and again by the sandbox at runtime — the compile-time check is a convenience, not the security boundary.

## Status

Early. The architecture is settled; implementation is in progress.

Design rationale lives in the code, not in a separate design document — each
crate's module docs explain what it is responsible for and why it looks the way
it does. Start at the [API docs][api-docs], or read `lib.rs` of
`crates/cuttlefish-host` for the core of the system. See
[AGENTS.md](./AGENTS.md) for why the project is organized that way.

## Development

The toolchain is pinned with Nix, which supplies the exact Rust (host and `wasm32-unknown-unknown` targets) and Python versions the project expects:

```console
$ nix develop                              # drops you into a shell with everything
$ nix develop --command cargo test --workspace
```

Running `cargo` outside that shell will pick up whatever toolchain happens to be on your `PATH`, which is a reliable source of confusing errors. If you'd rather not use Nix, check `flake.nix` for the pinned versions and match them yourself.

Test coverage is measured with [`cargo-llvm-cov`][llvm-cov] and reported to Codecov on every push:

```console
$ nix develop --command cargo llvm-cov --workspace --all-features
```

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](./CONTRIBUTING.md) for
how the project is built and tested, and
[CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md) for the standards expected of
participants. Security issues should follow [SECURITY.md](./SECURITY.md) rather
than being filed as public issues.

Two conventions worth knowing before you start, both covered in
[AGENTS.md](./AGENTS.md):

- **Documentation lives in the code**, as rustdoc. There is no `docs/` tree —
  it is gitignored. Explanations belong next to what they explain, where review
  catches them going stale.
- **Comments explain *why*, never *what*.** Several of this codebase's choices
  exist to avoid failures that are invisible from reading the result. Preserve
  and extend those rather than tidying them away.

## Prior art and acknowledgements

Cuttlefish stands on work that came before it:

- [**rune**](https://github.com/hotg-ai/rune) (hotg-ai) — the "declarative spec compiles to a single portable wasm binary" model, typed dataflow pipelines, and versioned addressable processing blocks all come from rune. Cuttlefish is a successor in that spirit, retargeted from edge ML inference to agent job delegation.
- [**llama.cpp**](https://github.com/ggml-org/llama.cpp) — local model inference.
- [**Wasmtime**](https://github.com/bytecodealliance/wasmtime) and the [Bytecode Alliance](https://bytecodealliance.org/) — the WebAssembly runtime and the sandboxing model.
- [**superpowers**](https://github.com/obra/superpowers) (obra) — the agent-harness patterns: discovery metadata that states *when* to use a tool rather than summarizing how it works, and a hard line between what is enforced by code and what is merely suggested in prose.

## License

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE_APACHE](./LICENSE_APACHE.md) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE_MIT](./LICENSE_MIT.md) or
   <http://opensource.org/licenses/MIT>)

at your option.

It is recommended to always use [`cargo crev`][crev] to verify the
trustworthiness of each of your dependencies, including this one.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

The intent of this crate is to be free of soundness bugs. The developers will
do their best to avoid them, and welcome help in analysing and fixing them.

[api-docs]: https://cuttlefishvm.github.io/cuttlefish-vm
[crev]: https://github.com/crev-dev/cargo-crev
[llvm-cov]: https://github.com/taiki-e/cargo-llvm-cov
