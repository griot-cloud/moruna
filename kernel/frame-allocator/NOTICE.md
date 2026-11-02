# OSDK frame allocator adaptation

The complete published `osdk-frame-allocator` 0.19.0 crate is retained in
`../vendor/osdk-frame-allocator-0.19.0`. Its source files retain their upstream
`SPDX-License-Identifier: MPL-2.0` notices and its Cargo license declaration.
The published crate does not contain a separate license-text file. The license
is available from <https://www.mozilla.org/MPL/2.0/>.

Moruna modifies the buddy-pool policy to keep all unbounded free buddy blocks
in one shared coalescing domain. The bounded per-CPU frame caches remain
unchanged. The optional `shared-pool-probe` feature exercises the same insertion
and allocation functions on genuine physical memory freed from actual CPUs.
It runs only in kernels built with the existing `probe` feature.

`upstream-files.json` records every file in the unmodified official crate.
`provenance.json` records the archive checksum and exact changed-file checksums.
No upstream dependency version, kernel ABI, Account ceiling, or process memory
limit changes as part of this adaptation. Runtime acceptance remains required.
