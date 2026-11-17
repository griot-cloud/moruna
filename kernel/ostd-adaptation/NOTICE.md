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
- **A stretched tick** (C5): `timer::set_tick_stretch` makes a CPU tick once every so many
  timer periods. The TSC deadline is armed that far ahead, and a smaller stretch re-arms
  it at once. The core stretches the tick of a CPU, other than the boot processor, that
  runs one thread or none.
- **Quiescent states without a task switch.** OSTD ends a grace period only when every CPU
  has switched tasks, so a CPU idling or running one thread held every grace period open,
  and every frame unmapped meanwhile stayed allocated. Returning to user mode now passes a
  quiescent state (`UserMode::execute`), as it does in Linux, and
  `sync::report_quiescent_state` lets the core's idle task pass one each time it wakes.

One more, marked `moruna:` (item 1.4, the system design's section 1B):

- **The counter at the core's first instruction.** Each boot protocol's entry
  (`__multiboot2_entry`, `__multiboot_entry`, `__pvh_entry`, `__linux_boot`) records the
  time-stamp counter first thing, and `boot::entry_tsc` gives it. The core's clock counts
  from it, so `boot_ms` includes OSTD's own start-up.

Three of its comments lose their em dashes (`src/log/logger.rs`, `src/log/macros.rs`,
`src/task/scheduler/mod.rs`), which the repository's quality check refuses in every
tracked file; their code is unchanged.

`upstream-files.json` records every file of the unmodified crate. `provenance.json`
records the archive's checksum and the changed files' checksums. No dependency, kernel
mapping, frame allocation or metadata changes.
