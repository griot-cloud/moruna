# Contributing to Moruna

Thanks for your interest. Moruna makes hard promises about memory, so a change has to hold on machines that are not yours. Everything below exists to make that checkable.

## Getting a change in

1. Fork, branch from `main`, open a pull request. `main` is protected: it takes pull requests with a green pipeline.
2. Run `tools/hooks/install.sh` once in your clone. The pre-commit hook runs the same gate CI runs, so a commit that would fail CI fails locally first.
3. Keep the crate you touch at 90% line coverage or better. The gate judges coverage per crate.

## What we look for

**Behaviour is specified before it is written.** [`architecture/`](architecture/) is the specification: [the architecture document](architecture/moruna-runtime-design.md) for the system, and [`architecture/sdd/`](architecture/sdd/) for each component in enough detail to rebuild it. If your change alters what the runtime does, update the document in the same pull request and say which section. If the code and a document disagree, that is a bug in one of them, and the fix starts with deciding which.

**A test should prove a property, not a machine.** An assertion like "this finishes in 20 microseconds" or "this run fits in 512 MiB" describes the laptop it was written on, and it fails for the next person without telling them anything true. Prefer assertions that hold anywhere: that submission returned before the work finished, that the peak never passed the ceiling, that the sink wrote exactly the sequence numbers the trace says were committed. Where a figure genuinely needs particular hardware, tag it for the reference host so it reports instead of asserting.

**A budget assertion needs a cgroup of its own.** Moruna sizes a run against the memory its own cgroup already holds, which is the run itself in a pod and the whole machine on a shared CI runner. Tests that assert a budget therefore run in the container stage, where the cgroup belongs to the test.

**No stubs in shipping source.** `tools/quality/no_stubs.sh` refuses `todo!`, `unimplemented!`, a panic whose message admits it, `NotImplementedError`, or a crate with no code. Tests are exempt, since a test tagged for hardware we do not have is meant to exist without running.

## House rules

- `cargo fmt` and `cargo clippy -- -D warnings` clean.
- No `unwrap` or `expect` outside tests; errors are typed and propagate.
- Every `unsafe` block carries a `// SAFETY:` comment naming the invariant that makes it sound, in a module its component's design document permits.
- Tests are named after the design document's test specification, prefix and id and a short slug (`pl_t4_fifo` for PL-T4), so a reviewer can map them.
- Tests write only to a scratch directory unique to the running process, never a fixed path.

## Releases

A release is a pull request that bumps the version in `Cargo.toml` and adds its section to
[CHANGELOG.md](CHANGELOG.md). Merging it is the release: CI builds the wheels for every platform,
publishes them to PyPI, and creates the tag afterwards, so a tag always names something that
shipped. Nobody creates tags by hand, and a merge that does not change the version publishes
nothing.

## Reporting a problem

Please include the host, the budget you gave the run, and what the run report said. The report is a pure function of the per morsel trace, so `report.to_json()` (and the trace file, where you can share it) usually answers the question outright. For anything with a security angle, follow [`SECURITY.md`](SECURITY.md) rather than opening a public issue.

## Licence and sign-off

By contributing you agree your contributions are licensed under the Apache License 2.0, and you confirm you have the right to contribute them ([the Developer Certificate of Origin](https://developercertificate.org/)). Sign off your commits with `git commit -s`.

Much of this codebase was written by coding agents working from the documents in `architecture/`, committing as `Moruna Agent <agents@griotdata.com>`. The maintainer takes DCO responsibility for those commits and reviews them against their design documents. Human contributors sign off in their own name as usual.

## Conduct

This project follows the Contributor Covenant; see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
