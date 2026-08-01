# Cuttlefish VM

<p align="center">
  <a href="https://cuttlefishvm.github.io"><img src="https://cuttlefishvm.github.io/assets/images/logo.png" alt="Logo" height=170></a>
  <br />
  <br />
  <a href="https://github.com/cuttlefishvm/cuttlefish-vm/actions/workflows/ci.yml" target="_blank"><img alt="Continuous Integration" src="https://github.com/cuttlefishvm/cuttlefish-vm/actions/workflows/ci.yml/badge.svg?branch=main" /></a>
  <a href="https://cuttlefishvm.github.io/cuttlefish-vm/" target="_blank"><img alt="Coverage" src="https://img.shields.io/endpoint?url=https%3A%2F%2Fcuttlefishvm.github.io%2Fcuttlefish-vm%2Fcoverage.json" /></a>
  <a href="https://crates.io/crates/cuttlefish" target="_blank"><img alt="crates.io" src="https://img.shields.io/crates/v/cuttlefish.svg" /></a>
  <a href="https://docs.rs/cuttlefish" target="_blank"><img alt="docs.rs" src="https://docs.rs/cuttlefish/badge.svg" /></a>
  <img alt="MSRV" src="https://img.shields.io/badge/rustc-1.94+-blue.svg" />
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
  model = Ollama "llama3.2:1b";
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

Early, but it runs end to end. A `.cuttlefish` spec drives a sandboxed wasm
block through the daemon and returns a structured result, with capabilities
enforced and output streamed:

```console
$ cuttlefish run --spec summarize_docs --input '{"path": "examples/docs/a.txt"}'
{
  "result": {
    "path": "examples/docs/a.txt",
    "summary": "The cuttlefish, also known as sepia or sea pen, is a marine animal
                recognized by its ability to rapidly change color and texture to
                blend in with its surroundings."
  },
  "status": "completed",
  "usage": { "duration_ms": 1628, "model": "llama3.2:1b", "tokens_in": 46, "tokens_out": 44 }
}
```

That is a real local model, not a stub. Still to come: more inference
providers, the typed DSL with block signatures, multi-block pipelines, the
model pool, the block registry, and the agent harness.

### Beyond text

A block is told what it opened, and branches on it:

| Input | What a block does |
|---|---|
| Text | `Slice` windows, as before |
| Image | names the handle in `Infer` — the host feeds the bytes to a vision model |
| PDF with a text layer | `PageText`, and any text model can read it |
| PDF without one (a scan) | `PageImage` renders the page, and a vision model reads it |
| Anything else | `SliceBytes` for raw bytes |

`Open` reports `pages` and `has_text_layer` for documents, so a block takes the
cheap path when it exists and the expensive one when it must — rather than
extracting nothing from a scan and summarizing the empty string.

Images never enter guest memory. A block names a *handle* in its inference
request and the host loads the bytes, so a 40 MB scan costs the guest nothing —
the same rule that governs file contents.

Rendering PDF pages to images needs the `pdf-render` feature (pdfium is a large
native dependency); text extraction is always available.

### Inference providers

A spec names a provider and a target; which backends exist is a property of the
build, not of the language:

| Provider | Target | Status |
|---|---|---|
| `Ollama` | model tag, e.g. `llama3.2:1b` | available |
| `LlamaCpp` | path to a `.gguf` file | available behind the `llamacpp` feature |
| `Stub` | the canned reply to return | available — deterministic, for testing pipelines without a model |
| OpenAI-compatible HTTP | endpoint URL | planned; covers llama.cpp's server, vLLM, LM Studio, hosted providers |

Adding one means implementing `InferBackend` and registering a factory — the
spec parser, the runner, and the daemon are untouched. Naming a provider this
build does not have produces an error listing the ones it does.

Set `OLLAMA_HOST` to point at a non-default Ollama.

**Embedded llama.cpp** is opt-in because it compiles llama.cpp from source,
which needs cmake, a C++ toolchain, and libclang:

```console
$ nix develop --command cargo build -p cuttlefishd --features llamacpp
```

It runs the model inside the daemon — no second process, and each job gets its
own context so nothing carries between them. Ollama needs none of that
toolchain, so prefer it unless you specifically want the model in-process.

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

To run the example job end to end:

```console
$ nix develop --command bash -c '
    cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown &&
    cargo build -p cuttlefishd -p cuttlefish &&
    ./target/debug/cuttlefishd examples/summarize.cuttlefish \
      target/wasm32-unknown-unknown/debug/cf_block_echo_summarize.wasm /tmp/cf.sock &
    until [ -S /tmp/cf.sock ]; do sleep 0.1; done
    ./target/debug/cuttlefish run --socket /tmp/cf.sock --spec summarize_docs \
      --input "{\"path\": \"examples/docs/a.txt\"}"
  '
```

The daemon serves over a unix domain socket, so it is unix-only for now; the
library crates are cross-platform and tested on Windows too.

Running `cargo` outside that shell will pick up whatever toolchain happens to be on your `PATH`, which is a reliable source of confusing errors. If you'd rather not use Nix, check `flake.nix` for the pinned versions and match them yourself.

Test coverage is measured with [`cargo-llvm-cov`][llvm-cov]:

```console
$ nix develop --command cargo llvm-cov --workspace          # summary
$ nix develop --command cargo llvm-cov --workspace --html   # browsable report
```

That is the same command CI runs. There is no coverage service and no upload
token — CI publishes the resulting percentage as a small JSON file alongside the
API docs, and the badge above renders from it. Coverage data never leaves the
build.

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
