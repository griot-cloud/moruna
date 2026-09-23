# Contributing to Moruna

Thank you for considering a contribution. Moruna is designed before it is built, so the most valuable contributions at this stage are to the design documents, and code contributions are expected to trace back to them.

## How the project works

- The documents in `architecture/` are authoritative. If a change in behaviour is not in a design document, it is not a change we can merge; open a pull request against the document first.
- Each component has a software design document (SDD) with numbered invariants and a test specification. Code for a component is reviewed against its SDD: every invariant must hold, every listed test must exist and pass.
- The runtime is Rust; the user surface is Python through PyO3. Kernel authors write against `moruna-kernel` only.

## Before you merge

The project does not use pull requests: one maintainer and a set of coding agents work here, and a pull request with no second reviewer is ceremony that also costs a CI run. An agent pushes its branch and reports; the PM agent reviews the branch's diff against `architecture/agents/report-template.md` and the checklist in `architecture/agents/pm.md`, then merges to `main`. An outside contributor opens a pull request as usual, and the same review applies.

1. Read `architecture/README.md` and the SDD for the component you are touching.
2. Open an issue describing the change and which SDD sections it affects, unless the change is a typo or a documentation fix.
3. Keep a branch to one component. A change that crosses components changes the contracts crate first, on its own branch.
4. Branch from `main`. Branch names: `component/NN-<slug>` with the slug the crate suffix (`component/02-arena`), `infra/<topic>` for the workspace skeleton, CI and bench work, `contracts/<topic>` for a change to `architecture/sdd/01-contracts.md` and the contracts crate. A component branch is merged by the PM agent when its gate is green (preamble section 6.6).
5. Fill every section of `architecture/agents/report-template.md`, including "Environment facts verified", "Tests skipped (id, reason)" and "Provisional results (host)"; an empty section is a review finding.

## Code expectations

- `cargo fmt` and `cargo clippy -- -D warnings` clean.
- No `unwrap` or `expect` outside tests; errors are typed and propagate.
- `unsafe` blocks carry a `// SAFETY:` comment stating the invariant that makes them sound, and are limited to the modules the SDD permits.
- Tests are named after the SDD test specification in snake case, prefix, id and a short slug (for example `pl_t4_fifo` for PL-T4), so review can map them.
- Every crate with code has at least 90% line coverage (`cargo llvm-cov`, test code excluded), judged per crate. `tools/quality/check.sh` is the gate; run `tools/hooks/install.sh` once per clone so the pre-commit hook runs it, and do not bypass the hook. CI runs the same script.
- Benchmarks state the machine they were measured on.

## Documents

- No em dashes; use commas, colons or parentheses.
- Prose between tables; a document that is only tables is not a design.
- Every current-state claim names where it was verified or the command that would verify it.

## Licence

By contributing you agree that your contributions are licensed under the Apache License 2.0, and you confirm you have the right to contribute them (the Developer Certificate of Origin, https://developercertificate.org/). Sign off your commits with `git commit -s`.

## Agent identity and sign-off

Much of the code is written by coding agents driven from the documents in `architecture/` (the prompts are in `architecture/agents/`). Agents commit as `Moruna Agent <agents@griotdata.com>` and sign off with `git commit -s`. That sign-off is made on behalf of the project by its maintainer, who takes responsibility under the Developer Certificate of Origin for what the agents commit, reviews every agent pull request against its design document, and merges or delegates the merge as the preamble's section 6.7 states. A human contributor signs off in their own name as usual.

## Conduct

This project follows the Contributor Covenant; see `CODE_OF_CONDUCT.md`.
