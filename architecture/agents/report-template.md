# Executor report template

What an executor hands the PM when its feature is done. It was the pull request template; the project does not open pull requests (preamble 6.7), so the same content is the report, and the PM reviews the branch diff against it before merging to `main`.

## What this changes

## Which design document and section it traces to

<!-- e.g. architecture/sdd/09-placement.md, PL-I3 and PL-T7 -->

## Invariants and tests by id

<!-- every invariant of the SDD's section c that this PR upholds, and every test of section k it adds; the PM checks each against the document -->

## Environment facts verified

<!-- each "environment fact to verify before starting" from the SDD's section l, with the command run and its result; filled before coding -->

## Tests skipped (id, reason)

<!-- only tests tagged "(integration, closes in wave N)" or "(reference host, E1)" in the SDD; an untagged test may not be skipped -->

## Provisional results (host)

<!-- every timing figure measured on a host other than the reference host (preamble E1), with that host's name; "none" if none -->

## Escalations (id, what, who decides)

<!-- every preamble section 7 item this work hit: the id (E1..E13), what was found (document and section, what is wrong or missing), and who decides per the table; a human-level item also has a design-change issue linked here; "none" if none -->

## Checklist

- [ ] The behaviour is described in a design document (or this PR changes the document)
- [ ] Tests named as in the SDD test specification exist and pass
- [ ] `cargo fmt` and `cargo clippy -- -D warnings` are clean
- [ ] Commits are signed off (`git commit -s`)
- [ ] No `unwrap`/`expect` outside tests; every `unsafe` has a `// SAFETY:` comment and lives in a module the SDD's section l permits
- [ ] No wildcard arm over `Tier` or `StagingCodec` (`tools/lint/no_tier_wildcard.sh` passes)
- [ ] Every crate this PR adds to a `Cargo.toml` is in the preamble's dependency table (section 6.2)
- [ ] No em dashes in documentation
- [ ] `tools/quality/check.sh` passes: every crate this PR touches has at least 90% line coverage, judged per crate
