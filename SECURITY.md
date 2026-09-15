# Security policy

Morsel is a runtime that reads files, allocates pinned memory, issues direct IO, and executes user-supplied kernels. Bugs in those paths can have security consequences, and we want to hear about them privately first.

## Reporting

Email security@griotdata.com with a description, the version or commit, and steps to reproduce. Please do not open a public issue for a suspected vulnerability. You will receive an acknowledgement within three working days and a resolution plan within ten.

## Scope

In scope: memory safety in the runtime and adapters, the arena and placement engine, file format parsing (Arrow IPC, Parquet, safetensors, the aligned binary format), the Python boundary, and any path that could let one kernel read another's morsels.

Out of scope: vulnerabilities in user-written kernels, and in upstream dependencies (report those upstream; tell us as well so we can pin a fixed version).

## Supported versions

Until a 1.0 release, only the `main` branch receives fixes.
