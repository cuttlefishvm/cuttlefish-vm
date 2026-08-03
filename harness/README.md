# Agent test harness: block catalog

Files here are for handing off to an agent to independently verify the
block-catalog feature (`cuttlefish catalog add/list/show/rm`), separate from
the implementation's own test suite (`cargo test -p cuttlefish-host`).

- **`catalog-cli-reference.md`** — the CLI surface, exact command syntax,
  and expected output shapes. Give this to an agent so it doesn't need to
  read the Rust source to know how to drive the tool.
- **`test-prompt.md`** — a ready-to-paste prompt asking an agent to verify
  the happy path and then adversarially try to break it (duplicate adds,
  typo lookups, corrupt index, concurrent writes, missing signatures). Copy
  the prompt block from that file directly to a fresh agent session.

Quick human-driven equivalent, if you'd rather just watch it run yourself
first: `./try-catalog.sh` from the repo root (mirrors the existing
`try-it.sh` for the daemon path, but exercises the catalog instead — no
daemon or Ollama needed).
