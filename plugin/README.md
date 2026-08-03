# cuttlefish-agent-tools

A Claude Code plugin: a `cuttlefish-catalog` skill (exact CLI syntax and
output shapes for `cuttlefish catalog add/list/show/rm`) plus a
`/cuttlefish-agent-tools:test-catalog` command that drives an agent through
an independent, adversarial exercise of the catalog CLI.

## Install (from this repo)

```
/plugin marketplace add ./
/plugin install cuttlefish-agent-tools@cuttlefish-vm
/reload-plugins
```

Then the `cuttlefish-catalog` skill loads automatically whenever it's
relevant, and `/cuttlefish-agent-tools:test-catalog` is available to run on
demand.
