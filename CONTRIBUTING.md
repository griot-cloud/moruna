# Contributing to Amoru

Thank you for considering a contribution. Amoru is designed before it is built, so the most valuable contributions at this stage are to the design documents, and code contributions are expected to trace back to them.

## How the project works

- The documents in `architecture/` are authoritative. If a change in behaviour is not in a design document, it is not a change we can merge; open a pull request against the document first.
- Each component has a software design document (SDD) with numbered invariants and a test specification. Code for a component is reviewed against its SDD: every invariant must hold, every listed test must exist and pass.
- The runtime is Rust; the user surface is Python through PyO3. Kernel authors write against `morsel-kernel` only.

## Before you open a pull request

1. Read `architecture/README.md` and the SDD for the component you are touching.
2. Open an issue describing the change and which SDD sections it affects, unless the change is a typo or a documentation fix.
3. Keep pull requests to one component. A change that crosses components changes the contracts crate first, in its own pull request.

## Code expectations

- `cargo fmt` and `cargo clippy -- -D warnings` clean.
- No `unwrap` or `expect` outside tests; errors are typed and propagate.
- `unsafe` blocks carry a `// SAFETY:` comment stating the invariant that makes them sound, and are limited to the modules the SDD permits.
- Tests are named as in the SDD test specification (for example `PL-T4`) so review can map them.
- Benchmarks state the machine they were measured on.

## Documents

- No em dashes; use commas, colons or parentheses.
- Prose between tables; a document that is only tables is not a design.
- Every current-state claim names where it was verified or the command that would verify it.

## Licence

By contributing you agree that your contributions are licensed under the Apache License 2.0, and you confirm you have the right to contribute them (the Developer Certificate of Origin, https://developercertificate.org/). Sign off your commits with `git commit -s`.

## Conduct

This project follows the Contributor Covenant; see `CODE_OF_CONDUCT.md`.
