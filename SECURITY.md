# Security policy

Moruna is a runtime that reads files, allocates pinned memory, issues direct IO, and executes user-supplied kernels. Bugs in those paths can have security consequences, and we want to hear about them privately first.

## Reporting

Email security@griotdata.com with a description, the version or commit, and steps to reproduce. Please do not open a public issue for a suspected vulnerability. You will receive an acknowledgement within three working days and a resolution plan within ten.

GitHub private vulnerability reporting (the "Report a vulnerability" button on the repository's Security tab) is the second private channel and is preferred when you already have a GitHub account, because it keeps the report, the fix and the advisory in one place. GitHub offers it on public repositories only, so until this repository is public the email address above is the one channel.

## What happens next

We work to coordinated disclosure. After the acknowledgement and the plan, we aim to have a fix released within ninety days of the report, and we will tell you if that is going to slip and why. We will not disclose the report publicly before a fix is available unless the issue is already public or is being exploited. Tell us in your first message how you would like to be credited, or that you would rather not be; the advisory names reporters who want to be named.

## How a fix reaches you

Moruna is distributed as wheels on PyPI and as crates on crates.io. A security fix ships as a new release of both, announced in a GitHub security advisory for this repository, in the release notes and in `CHANGELOG.md`. A wheel is never patched in place: upgrade to the fixed version. Watch the repository's releases, or the advisory feed, if you want to hear about these without asking.

## Scope

In scope: memory safety in the runtime and adapters, the arena and placement engine, file format parsing (Arrow IPC, Parquet, safetensors, the aligned binary format), the Python boundary, and any path that could let one kernel read another's morsels.

Out of scope: vulnerabilities in user-written kernels, and in upstream dependencies (report those upstream; tell us as well so we can pin a fixed version).

## Supported versions

Until a 1.0 release, only the `main` branch receives fixes.
