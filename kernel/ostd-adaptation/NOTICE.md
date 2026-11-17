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

It also adds what a node needs to stop on every machine (`architecture/moruna-os-system.md`,
section 1B; work item 1.2). Each patch is marked with a `moruna:` comment:

- **ACPI S5 power-off** (`src/arch/x86/power.rs`). OSTD powered off only through QEMU's
  test exit device, which no cloud has. `try_poweroff` now tries that device first, as
  before, and then ACPI S5: SLP_TYPa from the DSDT's `\_S5` package, written with SLP_EN to
  the FADT's PM1a control register (an I/O port). After the write it waits, within the
  boot waits' deadline below, for a hypervisor that acts on it a moment later. If neither
  powers the machine off it returns, and OSTD halts every CPU as it did. A power-off before
  `power::init` has run (a boot wait whose deadline passed) writes the test exit device's
  port too, when the CPU reports QEMU. S5 is not tried before the IRQ chip is up: until
  then a panic may come from reading the ACPI tables, which must not be entered twice.
- **Deadlines on the two calibration waits** (`src/arch/x86/kernel/tsc.rs`,
  `src/arch/x86/timer/apic.rs`). The PIT calibration of the TSC and of the APIC timer each
  halted until the next PIT interrupt, for ever on a machine with no PIT. Each now pauses
  with local IRQs enabled instead, and gives up once the TSC has counted more than 2^33
  from where the wait began (about 2 s at any rate a cloud CPU runs, counted without the
  rate the wait is there to measure). It then prints
  `moruna: fatal core: PIT calibration timed out` or
  `moruna: fatal core: APIC timer calibration timed out` and powers the machine off.

The arithmetic of both is in two added files that name nothing of OSTD:
`src/arch/x86/power/acpi_s5.rs` (the scan of a DSDT for `_S5_`, the package's first
element, the PM1a value) and `src/arch/x86/kernel/tsc/boot_wait.rs` (the wait and its
deadline). This directory is also the crate `moruna-ostd-adaptation`, which compiles
those same two files for the host and tests them (`cargo test -p moruna-ostd-adaptation`).
They carry `SPDX-License-Identifier: MPL-2.0`, as the files beside them do.

Three of its comments lose their em dashes (`src/log/logger.rs`, `src/log/macros.rs`,
`src/task/scheduler/mod.rs`), which the repository's quality check refuses in every
tracked file; their code is unchanged.

`upstream-files.json` records every file of the unmodified crate. `provenance.json`
records the archive's checksum, the changed files' checksums and the added files'. No
dependency, kernel mapping, frame allocation or metadata changes.
