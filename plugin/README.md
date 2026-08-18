# Cuttlefish agent tools

Portable coding-agent skills for authoring, building, running, testing, and
managing Cuttlefish VM workloads. The plugin is packaged for Codex and
Claude Code, while the skills themselves use the standard `SKILL.md` format
and plain shell/CLI instructions.

## What it provides

Core workflow skills:

- `cuttlefish-cli` resolves current Cuttlefish binaries.
- `cuttlefish-author` scaffolds and writes Rhai or Rust blocks.
- `cuttlefish-build` links specifications into reproducible bundles.
- `cuttlefish-catalog` manages local block and bundle versions.
- `cuttlefish-run` drives project-scoped daemons and jobs.

Black-box verification skills:

- `cuttlefish-test-author`
- `cuttlefish-test-build`
- `cuttlefish-test-catalog`
- `cuttlefish-test-run`

The test skills isolate their state, exercise adversarial cases, and report
expected versus actual behavior without modifying product code.

## Install in Codex

Add the repository marketplace:

```console
$ codex plugin marketplace add cuttlefishvm/cuttlefish-vm
$ codex
```

Inside Codex, enter `/plugins`, select the `cuttlefish-vm` marketplace,
install **Cuttlefish VM**, and start a new session.

## Install in Claude Code

From anywhere:

```text
/plugin marketplace add cuttlefishvm/cuttlefish-vm
/plugin install cuttlefish-agent-tools@cuttlefish-vm
/reload-plugins
```

From a local checkout, replace `cuttlefishvm/cuttlefish-vm` with `./`.
Claude's existing `/cuttlefish-agent-tools:test-*` commands remain as thin
wrappers around the portable test skills.

## Use with other coding agents

Agents supporting the [Agent Skills](https://agentskills.io/) format can
load `plugin/skills/` directly or copy/symlink individual skill directories
into their configured skills location. Optional delegation hints can be
ignored when a runtime has no subagents. No Claude command, Codex hook, MCP
server, or hosted service is required; the core procedures drive the local
`cuttlefish` and `cuttlefishd` CLIs.
