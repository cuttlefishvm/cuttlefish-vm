# cuttlefish-agent-tools

A Claude Code plugin: a `cuttlefish-catalog` skill (exact CLI syntax and
output shapes for `cuttlefish catalog add/list/show/rm`) plus a
`/cuttlefish-agent-tools:test-catalog` command that drives an agent through
an independent, adversarial exercise of the catalog CLI.

It also carries a `cuttlefish-build` skill (exact CLI syntax and output
shapes for `cuttlefish build <spec> [-o out.cfbundle]`, the pipeline linker
that resolves each stage — direct path or catalog `name@version` alike,
checks the seams between blocks, and packages the result into a
distributable `.cfbundle`) plus a `/cuttlefish-agent-tools:test-build`
command that drives an agent through an independent, adversarial exercise
of the build CLI.

These skills are plain bash/CLI instructions with no Claude-Code-specific
tool references — usable by any coding agent that can run shell commands
and read Markdown, not just Claude Code.

## Install (from this repo)

```
/plugin marketplace add ./
/plugin install cuttlefish-agent-tools@cuttlefish-vm
/reload-plugins
```

Then the `cuttlefish-catalog` and `cuttlefish-build` skills load
automatically whenever they're relevant, and
`/cuttlefish-agent-tools:test-catalog` and
`/cuttlefish-agent-tools:test-build` are available to run on demand.
