# OSTD adaptation

The complete published `ostd` 0.19.0 crate is kept in `../vendor/ostd-0.19.0`, and
`kernel/Cargo.toml` patches the workspace to it. Its source files keep their upstream
`SPDX-License-Identifier: MPL-2.0` notices and its Cargo licence declaration. The
licence is available from <https://www.mozilla.org/MPL/2.0/>.

Moruna adds two of the data OS's mechanisms (`architecture/data-os.md` §2), both in
kernel mode and so counted against §0:

- **Large pages in a process's page table.** OSTD 0.19.0 maps a process's memory in
  4 KiB frames only. The patch lets a user page table hold a 2 MiB entry over a run of
  frames that are physically contiguous and aligned (`VmSpace::cursor_mut_huge`,
  `CursorMut::map_huge`). The entry holds one reference to each frame of its run, as
  OSTD's page table requires of an item that it splits: splitting the entry into
  4 KiB entries, unmapping it, and dropping its table each hand those references on or
  release them. `Segment::from_raw_unsized` restores such a run for an untyped frame.
- **Each CPU's number in `IA32_TSC_AUX`,** written as the CPU starts, so a thread reads
  the CPU it runs on with `rdtscp` and no call.

Three of its comments lose their em dashes (`src/log/logger.rs`, `src/log/macros.rs`,
`src/task/scheduler/mod.rs`), which the repository's quality check refuses in every
tracked file; their code is unchanged.

`upstream-files.json` records every file of the unmodified crate. `provenance.json`
records the archive's checksum and the changed files' checksums. No dependency, kernel
mapping, frame allocation or metadata changes.
