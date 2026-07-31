# Cuttlefish — design spec

Native tooling for agents: a local daemon that runs local models via llama.cpp, driven by a typed `Cuttlefish.spec`-equivalent DSL (successor in spirit to [f0rodo/rune](https://github.com/f0rodo/rune), itself a fork of the abandoned [hotg-ai/rune](https://github.com/hotg-ai/rune) TinyML/edge-ML wasm pipeline compiler — no LLM story existed there; this project supplies it from scratch).

## Motivation

From prior art review (this session):

- **hotg-ai/rune**: proved the "declarative spec compiles to a single portable wasm binary" shape, with typed dataflow DAGs and versioned, addressable pipeline components ("proc-blocks"). Dead upstream 4+ years, no LLM/job-dispatch story, no runtime service — just a compile-then-run CLI.
- **obra/superpowers**: proved that a coding-agent harness needs (a) trigger-only discovery metadata (a `description` that states *when* to use something, never *how* it works, or the agent skips the real logic), (b) a ledger/checkpoint file that survives context compaction, and (c) a hard split between what's *enforced* (deterministic code) and what's merely *suggested* (prose the model chooses to follow).
- **claurst_bridge** (local Rust crate, `dnav-preact-integration/rust/claurst_bridge`): proved a concrete async pattern for native-runtime-calls-into-sandboxed-code — `tokio::spawn` + `tokio::select!` event-draining loop, `CancellationToken` threaded through job context, `[patch]`-whole-tree vendoring for a forked upstream dependency, rustls-over-native-tls to dodge TLS symbol clashes.
- **Team chat (screenshot, this session)**: the core value proposition is privacy/cost — proprietary or personal data never has to leave the machine, and cheap/bulk subtasks don't burn frontier-model tokens. Explicitly acknowledged limit: local models can't match 1T+-parameter frontier models, so cuttlefish jobs are the subset of work that doesn't need frontier reasoning.

## Goals

Support three job shapes through one mechanism (spec → wasm → daemon):
1. **Cost/token offload** — cheap, repetitive, or bulk subtasks (summarize, classify, extract) pushed to a local model.
2. **Deterministic verified pipeline** — typed, reproducible, sandboxed steps (Rune's DAG shape), one or more of which happen to call a local LLM.
3. **Autonomous background subagent** — a longer independent task loop (multi-turn, its own tool use) that reports back a final result. *v1 scoping*: the DSL's pipeline layer is a static DAG with no loop construct, so in v1 an agent loop lives *inside* a single proc-block. This is less opaque than it sounds — under the reactor-model host ABI (see Daemon internals) every inference iteration surfaces to the host as a discrete command, so the loop remains observable, cancelable, and token-metered even though the DAG can't see its structure. A DAG-level loop/conditional construct, and the tool-invocation ABI "own tool use" needs, are follow-on spec material.

## Architecture

Two binaries sharing a `cuttlefish-core` Rust crate:

- **`cuttlefishd`** (daemon) — long-lived. Owns the wasm host (wasmtime), the pool of loaded llama.cpp model instances, the job queue/scheduler, and the local HTTP/socket API. This is what agents talk to at job-submission time.
- **`cuttlefish`** (CLI) — short-lived, talks to `cuttlefishd` over the same local API. Owns `build` (compiles a spec + its referenced proc-blocks into a single wasm binary), `run` (submit-and-wait convenience wrapper), proc-block registry commands (`push`/`pull`/`search`), `inspect`/`graph` (introspect a compiled spec's typed IR).

Rejected alternatives: a single multi-mode binary (Rune's own shape) couples the daemon's runtime dependency tree to build-time compiler deps; a daemon-only design with no CLI loses a coherent human-facing build/registry workflow. The split mirrors docker/dockerd — the daemon owns model residency and long-lived state, the CLI is disposable.

**Wasm/native split**: wasm program owns orchestration (prompt construction, response parsing/validation, looping for multi-turn/tool-use, deciding when a job is done). Native runtime owns model inference **and** capability-gated IO the sandbox can't safely do itself (see Data boundary below) — not "inference only" as originally scoped; file reads for `local_only` jobs are a deliberate, narrow exception so proprietary bytes never have to pass through the wasm sandbox via the calling agent.

## Agent harness (superpowers-derived)

Cuttlefish ships an agent-harness package alongside the daemon so a calling coding agent learns it exists and when to delegate:

- **Skills plugin** (multi-platform adapter dirs, mirroring superpowers). Core skill `using-cuttlefish`, injected at session start: tells the agent when to delegate — bulk/repetitive subtasks, and any step touching data flagged local-only.
- **Per-spec discovery**: `GET /specs` lists installed specs with their `description` field. That field is trigger-conditions-only ("Use when...") by convention — never a workflow summary, per superpowers' tested finding that summarizing lets the agent skip the real contract. A compile-time lint flagging a description that matches workflow-summary patterns is in scope for v1 (see Testing strategy).
- **Data boundary**: a spec declares `data_policy: local_only`. This field is discovery metadata only — consumed by the harness skill, which instructs the calling agent to pass file *paths*, not file contents, for such jobs, so `cuttlefishd` reads the files itself. `data_policy` carries no runtime enforcement of its own: the thing that actually gates `host_read` is the `capabilities` list (below), checked at compile time and again by the daemon at runtime. A spec can declare `Read` capability independent of `data_policy` — `data_policy: local_only` is what tells the *agent* to behave differently (paths not content); `capabilities` is what tells the *sandbox* what it's allowed to touch. The privacy guarantee is that the frontier-model agent never reads the proprietary bytes into its own context, not merely "we also ran something locally."
- **Enforcement boundary**: the wasm program + daemon are the *enforced* contract (deterministic, sandboxed, capability-checked). Spec/skill prose only governs the calling agent's *decision to delegate* — natural language never controls the runtime.
- **Sandbox default-deny**: a proc-block gets no network/filesystem access by default. The daemon grants only the capabilities a spec declares (see below), visible via `cuttlefish inspect`.

## Spec format — typed DSL

`.cuttlefish` files, OCaml-flavored (ADTs, `let`-bound pipeline, real type inference over block signatures) — chosen over YAML+JSON-Schema after comparing three options lifted from OCaml-world practice:

| Option | What it changes | Verdict |
|---|---|---|
| **A. Typed DSL** | Replaces the spec language itself; compiler unifies block I/O types, catches DAG mismatches at compile time | **Adopted for v1** |
| B. Dune/opam-style workspace + lockfile | Build tooling/project layout only, spec language unchanged | Deferred — revisit once a project has more than a couple of local blocks |
| C. Imandra-style formal verification | A `verify:` pass proving effect/capability properties, orthogonal to language choice | Deferred — needs an effect system on blocks first; A's compile-time effect check (below) covers the capability case for v1 |

Compiler implementation: **Rust**, not OCaml — a chumsky-based parser plus custom unification over block signatures, living in `cuttlefish-core`. One toolchain end to end (parse → typecheck → link wasm); only the *language design* is OCaml-flavored, not the implementation.

```ocaml
spec summarize_docs = {
  description = "Use when the agent needs summaries of local files or bulk
                  text and content must not leave the machine.";
  model = Hf "Qwen/Qwen2.5-7B-Instruct-GGUF#q4_k_m" ~ctx:8192;  (* or: Path "./models/foo.gguf" *)
  data_policy = Local_only;
  capabilities = [ Read "./docs" ];  (* v1: capability = Read of string; Network deferred *)

  pipeline =
    let chunks = block Chunk_text ~max_tokens:2000 (Input files) in
    let sums   = block (Map_infer ~prompt:"./prompts/summarize.txt") chunks in
    block Merge_summaries sums
  ;

  input  : { files : string list };
  output : { summary : string; per_file : (string * string) list };
}
```

`model` is a variant type — `Hf of string` (resolved + hash-cached by the daemon on first use) or `Path of string` (pre-provisioned, no fetch logic). Both are first-class; a spec author picks whichever fits their deployment.

Each proc-block ships a `.cfi` interface file (like an `.mli`): input/output types plus an effect signature (`reads: [...]`; more effect kinds as future capability kinds land). **`.cfi` signatures are parametric** — a block may be generic over element types (e.g. `Map_infer : ('a -> prompt) * 'a list -> summary list` style, HM type variables), because mapping blocks are inherently polymorphic and a monomorphic-only unifier could not type any `Map_*` block at all. The typechecker does first-order unification with type variables across the pipeline's `let`-bindings — a block-ordering mismatch, or a block whose declared effects exceed the spec's `capabilities`, is a compile error before wasm linking runs. This is checked again at runtime by the daemon as defense-in-depth (see Host ABI below) — the compile-time check is not a substitute for the sandbox.

**Build-time assets**: file arguments to blocks (e.g. `~prompt:"./prompts/summarize.txt"`) are resolved and **embedded into the wasm artifact at build time** — they are compile-time inputs, not runtime reads, so they need no `Read` capability and the sample above does not violate its own `Read "./docs"` grant. Runtime reads are exclusively the job-input paths flowing through the `Read` capability.

Block refs resolve from a **versioned remote registry** (`org/repo@version#block`, fetched + cached + hash-verified, chosen from day one over local-only to support cross-project reuse) or a local path — both produce the same artifact format, so promoting local → published is just `cuttlefish push`.

`cuttlefish build` output: one linked wasm module (blocks + generated DAG-walking driver) plus an embedded manifest (model ref, capabilities, schemas). `cuttlefishd` loads that artifact directly; `inspect`/`graph` read the manifest and typed IR back out.

## Daemon internals

**API** (unix socket by default, TCP optional):
```
POST   /jobs              {spec: name, input: {...}}         -> {job_id}
GET    /jobs/:id          current status + result envelope if finished (poll/recovery path)
GET    /jobs/:id/events    SSE stream: progress | tokens | result | error
DELETE /jobs/:id          cancel
GET    /specs             [{name, description, input_schema}]  (harness discovery)
GET    /models            loaded models + memory budget status
```

Results are **not** delivered exclusively over SSE: a finished job's envelope is retained by the daemon (until acknowledged or TTL-expired) and fetchable via `GET /jobs/:id`, and the SSE endpoint honors `Last-Event-ID` for replay — a dropped connection right after the `result` event can never lose the result permanently.

**Result envelope** (fixed, spec-independent): `{status, result: <output-typed payload>, error?, usage: {tokens_in, tokens_out, duration_ms, model}}`.

**Model pool** — keyed by resolved model ref (hash of gguf + quant). llama.cpp separates immutable **model weights** (`llama_model`, shareable) from per-conversation **contexts** (`llama_context`, which own the KV cache). The pool exploits that split:

- Weights load once per model, shared read-only across jobs.
- Each job gets its **own `llama_context`** — KV cache is never shared, so no prompt/conversation state can leak between jobs, and a multi-turn job keeps its KV cache warm across its own `Infer` commands (no quadratic re-prefill).
- The memory budget (`--vram-budget`/`--ram-budget`) counts **weights + each job's KV allocation** (KV at ctx 8192 can run to GBs — it is not free relative to weights and must be admission-checked). A job is admitted only if its model's weights (if not yet loaded) *plus* its context's KV fit the remaining budget; otherwise it queues.
- Eviction targets least-recently-used *idle* weights (no live contexts); a model with any in-flight job is never evicted.
- Jobs on different models run in true parallel. Multiple jobs on the same weights also run in parallel (separate contexts); the daemon serializes only the actual decode calls per device as needed. Batched multi-sequence decoding (llama.cpp seq-ids, one context multiplexing jobs) is an explicit **non-goal for v1** — it shares context state across jobs and would compromise the isolation story for a throughput win we don't need yet.

**Scheduler / job lifecycle** — direct lift from claurst_bridge's async pattern: each job is a `tokio::spawn`ed task running one wasmtime instance (no shared mutable state between jobs) plus a `CancellationToken`. Host ABI calls go through `tokio::select!` against that token, so cancel works mid-inference, not just between pipeline steps. States: `queued → running → completed | failed | cancelled`. A job queues on model-pool availability, not a global lock.

**Host ABI — reactor model, not blocking calls.** A naive `host_infer_stream(..., on_token: fn)` import is incoherent for single-threaded core wasm: if the call returns a handle immediately there is no execution context in which the host can invoke `on_token` (the guest isn't running); if it blocks, the handle and any cancel import are dead API. Wasmtime `Store`s are also `!Sync` while llama.cpp inference must run on a blocking thread, and core wasm can't pass `fn` across the boundary at all.

So control is inverted (the wasmCloud/Spin actor pattern): the guest is a **state machine with multiple exports, driven by the host**. The guest never blocks on the host; it returns *commands*, the host executes them and re-invokes the guest:

```
; guest exports (called BY the daemon)
init(input_json) -> state
step(state, event) -> command
on_token(state, token) -> TokenAction    # optional export: streaming decisions (Continue | Stop)

; command = the guest's request to the host, returned from step():
;   Infer  { model, prompt, params }     -> host runs inference, feeds tokens to on_token
;                                            (if exported), then calls step(state, InferDone{text, usage})
;   Read   { path }                      -> capability-checked, then step(state, ReadDone{bytes})
;   Emit   { progress_json }             -> forwarded to the job's SSE stream, then step(state, Emitted)
;   Done   { result_json }               -> job completes with this payload
```

Consequences, all of which fall out for free:
- **Streaming**: tokens cross a channel from the llama.cpp thread to the store-owning task, which invokes the `on_token` export per token — host-driven, no callback-into-blocked-guest problem.
- **Cancel**: the daemon simply stops stepping the instance and aborts any in-flight inference via its `CancellationToken` — no guest-side cancel ABI needed at all.
- **Loops are natural**: a guest returning `Infer` commands repeatedly *is* a multi-turn loop, and every iteration passes through the host — observable, cancelable, token-countable. (See Goals note below on job shape 3.)
- The `cuttlefish-sdk` crate hides the state machine behind ordinary-looking Rust (an async-style API compiled to the step/command form), so block authors don't hand-write state transitions.

Capability checks happen at command-execution time: a `Read` outside the spec's declared capabilities, or any command with no corresponding grant, fails the job immediately — fail-closed, matching claurst_bridge's `OcamlPolicyHandler` posture.

**Scoping note on capabilities**: v1's capability type is `Read of string` only. `Network` was considered but is cut — the v1 command set has no network command, so a declarable-but-unexercisable capability would be dead surface. Network commands and a tool-invocation command (needed for job shape 3's "own tool use") land together in a follow-on spec.

**Errors** — structured codes in the envelope's `error` field: `model_load_failed`, `capability_denied`, `schema_validation_failed`, `wasm_trap`, `timeout`, `cancelled`. No silent partial results — a trap or timeout always yields `status: failed`, never a truncated `result`.

## Testing strategy

- **Compiler**: golden-file tests — `.cuttlefish` fixtures paired with expected typed IR or expected error (signature mismatch, capability-exceeds-declared, unresolved registry ref). No wasm/network involved.
- **Proc-blocks**: native unit tests against `.cfi` types, plus one wasmtime-harness integration test per block driving the reactor exports (`init`/`step`/`on_token`) with canned command results — proves state-machine/sandbox correctness without a live model.
- **Daemon**: integration tests with a stub model backend (scripted token streams, no real llama.cpp) covering queueing under budget pressure, eviction-never-kills-in-flight, cancel-mid-inference, capability-denial traps, malformed-input rejection.
- **End-to-end**: a small quantized model (e.g. 0.5B GGUF) in CI for one or two full specs, asserting envelope shape and basic correctness — not output quality, which is a separate non-blocking eval suite.
- **Harness/discovery**: a lint that flags a `description` matching workflow-summary patterns, plus one integration test that a real agent-shaped client can discover, submit, and stream a job end to end.

## Explicitly deferred

- Dune/opam-style workspace + lockfile tooling for multi-block projects (Option B above).
- Imandra-style formal verification pass beyond the compile-time effect/capability check already in the typechecker (Option C above).
- Multi-language proc-block authoring (WASI/WIT component model) — v1 is Rust-only via a `cuttlefish-sdk` crate.
- `Network` capability + network commands, and a tool-invocation command for job shape 3's "own tool use" — land together in a follow-on spec.
- DAG-level loop/conditional constructs in the DSL (v1: loops live inside a proc-block via the reactor command loop).
- Distributed/orchestrated wasm execution (wasmCloud/Spin-style "wasm kubernetes" — many daemon nodes, actor placement, lattice networking). v1 deliberately adopts only the *invocation pattern* from that world (host-driven multi-export reactor, which is what makes the host ABI sound); the distribution layer itself is a separate concern the reactor model leaves the door open to, since a stepped state machine with explicit commands is exactly what a distributed scheduler can checkpoint and migrate.

Note: streaming + cancel are **in v1**, not deferred — see the reactor-model Host ABI above (`on_token` export + host-side cancellation) and the recommended first plan below, which ends at a streamed result.

## Implementation planning scope

This document is the whole-system architecture pass, by design (chosen explicitly over sub-project-first specs, since these pieces share one compile→link→run pipeline and needed to be designed together to keep the wasm/native split and capability model consistent end to end). It is **not** meant to be planned and built as a single implementation pass — it bundles five subsystems (DSL compiler, daemon, CLI, block registry, agent-harness plugin) that don't need to land simultaneously.

Recommended first implementation plan: **compiler + daemon + CLI, local-path-only** — a minimal end-to-end job (`.cuttlefish` spec with `Path`-referenced model and local-path block refs, no registry network calls) that exercises the full architecture (typed DSL → wasm link → daemon load → host ABI → streamed result) without standing up registry infrastructure or the harness plugin. Registry (`push`/`pull`/hash-verify service) and the agent-harness skills plugin are each substantial enough to warrant their own follow-on spec once the core loop is proven.
