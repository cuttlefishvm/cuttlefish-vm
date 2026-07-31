# Security Policy

## Reporting a vulnerability

Please do not open a public issue for security vulnerabilities.

Email <kartik@thakore.ai> instead.

(Once this repository is public we intend to enable GitHub's private
vulnerability reporting and accept reports through Security → Report a
vulnerability. That feature is only available on public repositories, so for
now email is the route.)

Please include the affected version or commit, what an attacker gains, and a
reproduction if you have one. We'll acknowledge within a few days and keep you
updated as we work on a fix. If you'd like credit in the advisory, say so and
tell us how you'd like to be named.

## What counts as a vulnerability here

Cuttlefish runs untrusted-ish WebAssembly against local files and local models,
so the interesting boundary is what a processing block can reach. We are
especially interested in reports of:

- **Sandbox escape** — a block reading, writing, or executing anything outside
  what its spec's `capabilities` grant.
- **Capability bypass** — path traversal or symlink tricks that get a read
  outside a granted root, or any way to reach a host command without the
  corresponding grant.
- **Cross-job leakage** — one job observing another's data, including handles
  from one job naming resources in another, or model state carrying over
  between jobs.
- **Data-policy violation** — any path by which a `local_only` job's file
  contents escape the machine.
- **Denial of service that survives the job** — resource exhaustion a cancelled
  or completed job leaves behind.

Note that the compile-time capability check is a convenience, not the security
boundary. A block whose declared effects pass the typechecker but which then
exceeds them at runtime should be stopped by the sandbox; if it isn't, that's a
vulnerability regardless of what the spec said.

## Out of scope

- The quality, safety, or accuracy of model output.
- Attacks that require the operator to have already granted the capability
  being abused — a spec that declares `Read "/"` and then reads `/` is working
  as designed.
- Vulnerabilities in dependencies that don't affect Cuttlefish. Report those
  upstream; tell us if we should pin or patch around them.

## Supported versions

The project is pre-1.0 and moving quickly. Only the `main` branch and the most
recent release receive fixes.
