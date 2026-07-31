# Contributing to Cuttlefish

Thanks for your interest. This document covers how to get a working environment,
what the review bar is, and a few conventions that are specific to this codebase.

By participating you agree to abide by the [Code of Conduct](./CODE_OF_CONDUCT.md).

## Getting set up

The toolchain is pinned with Nix so that everyone builds against the same Rust,
the same wasm target, and the same Python:

```console
$ nix develop
$ nix develop --command cargo test --workspace
```

Every build and test command in this project's docs is written as
`nix develop --command ...`. That is not decoration — running `cargo` bare picks
up whatever is on your `PATH`, and version skew here produces errors that look
like code bugs. If you can't use Nix, read the pinned versions out of
`flake.nix` and match them.

Guest processing blocks compile to `wasm32-unknown-unknown`:

```console
$ nix develop --command cargo build -p cf-block-echo-summarize --target wasm32-unknown-unknown
```

## Before you open a pull request

```console
$ nix develop --command cargo fmt --all
$ nix develop --command cargo clippy --workspace --all-targets -- -D warnings
$ nix develop --command cargo test --workspace
$ nix develop --command cargo llvm-cov --workspace   # coverage, if you touched logic
```

CI runs all of these on Linux, macOS, and Windows, so it's cheaper to catch
failures locally.

## Conventions worth knowing

**Tests come first.** New behaviour should arrive with a test that fails before
your change and passes after. This matters more than usual here because much of
the system is a sandbox boundary — a capability check that silently stops
checking is not a visible failure, so the test *is* the enforcement.

**Comment the why, not the what.** Several parts of this codebase look strange
on purpose. The host drives the wasm guest rather than letting the guest call
back into the host; blocks receive file handles rather than file contents; the
guest returns a pointer to a descriptor struct rather than packing a pointer and
a length into one integer. Each of those avoids a specific failure that is
invisible once you're only reading the result. When you touch such code, keep
its comment. When you make a judgement call the next reader could plausibly
"simplify" away, add one.

Do not write comments that restate the code, and do not write comments
addressed to a reviewer ("changed this to fix the bug") — those go in the pull
request description, which is where they stay useful.

**Don't widen the sandbox casually.** Anything that grants a guest new reach —
a new command, a new capability kind, a new host function — needs an explicit
argument in the pull request for why it can't be done with what already exists.

## Commit messages and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `build:`, `ci:`. Keep the subject
under about 70 characters and use the body to explain *why*, not *what*.

Pull requests should describe the problem before the solution, and mention any
alternative you rejected and why — that context is usually the most valuable
part of the review.

## Reporting bugs

Open an issue with what you expected, what happened, and the smallest input that
reproduces it. Include the output of `nix develop --command cargo --version` and
your platform.

Security vulnerabilities are the exception: please follow
[SECURITY.md](./SECURITY.md) instead of filing a public issue.

## Licensing of contributions

Unless you state otherwise, contributions are dual-licensed under Apache-2.0 and
MIT, matching the project. See the License section of the [README](./README.md).
