## What and why

<!-- Describe the problem before the solution. If you rejected an alternative
     approach, say which and why — that context is usually the most valuable
     part of the review. -->

## Checklist

- [ ] `nix develop --command cargo fmt --all --check`
- [ ] `nix develop --command cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `nix develop --command cargo test --workspace`
- [ ] New behaviour has a test that fails without the change
- [ ] Rationale for non-obvious code is in a comment or module doc, not only in this PR description (see [AGENTS.md](../AGENTS.md))
- [ ] If this widens what a guest can reach, the PR explains why existing primitives were insufficient
