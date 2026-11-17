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
- **The same deadline on the wait for the other CPUs to start** (`src/boot/smp.rs`, on
  x86). The boot processor waited without end for every other CPU to report in. It now
  gives up at the same count, prints `moruna: fatal core: CPU start timed out` and powers
  the machine off. `src/arch/x86/mod.rs` gives the deadline's module the name
  `arch::boot_wait`, by which code outside `arch` reaches it.

Their arithmetic is in two added files that name nothing of OSTD:
`src/arch/x86/power/acpi_s5.rs` (the scan of a DSDT for `_S5_`, the package's first
element, the PM1a value) and `src/arch/x86/kernel/tsc/boot_wait.rs` (the wait, its
deadline and the three waits' lines).

It also adds what the core needs on Hyper-V (`architecture/moruna-os-system.md`, section
1F, "The core on Hyper-V"; work item 1.8). Each patch is marked with a `moruna:` comment:

- **Finding the hypervisor** (`src/arch/x86/kernel/hyperv.rs`, called from
  `late_init_on_bsp` in `src/arch/x86/mod.rs` before the clock rates are taken). CPUID
  leaf 1's hypervisor bit, then Microsoft's interface: leaves up to `0x40000005` and
  leaf `0x40000001` answering `Hv#1`, as the TLFS asks; otherwise KVM by its signature.
  It prints `moruna: hypervisor hyperv` or `moruna: hypervisor kvm`, and nothing on a
  machine with no hypervisor or another one.
- **Clock rates** (`src/arch/x86/kernel/tsc.rs`, `src/arch/x86/timer/apic.rs`). The TSC's
  rate comes from kvmclock, then CPUID leaf 0x15, then Hyper-V's
  `HV_X64_MSR_TSC_FREQUENCY` when leaf `0x40000003` says the frequency MSRs may be read
  (EAX bit 11 and EDX bit 8), and the PIT last. The APIC timer in periodic mode takes its
  initial count from `HV_X64_MSR_APIC_FREQUENCY` on Hyper-V and is calibrated against the
  PIT elsewhere, as before. Leaf 0x15's arithmetic moved, unchanged, from
  `src/arch/x86/cpu/cpuid.rs` (`query_tsc_freq`, removed with its `Leaf::Tsc`).
- **The PIT is looked for before it is used** (`src/arch/x86/timer/pit.rs`,
  `is_present`). Channel 0 is set counting and its latched count read until two readings
  differ, at most 1,024 times; channel 0's port is now read as well as written. A machine
  with no PIT, as a Hyper-V Generation 2 machine, prints
  `moruna: fatal core: no PIT to calibrate the TSC` or
  `moruna: fatal core: no PIT to calibrate the APIC timer` and powers off at once, where
  it waited out the deadline before.
- **The PIC is set up only where it answers its probe** (`src/arch/x86/irq/chip/mod.rs`).
  Upstream also set it up wherever the MADT sets PCAT_COMPAT.
- **The guest OS ID and the hypercall page** (`src/arch/x86/kernel/hyperv.rs`), on Hyper-V
  when the partition may use the hypercall MSRs (leaf `0x40000003` EAX bit 5). It writes
  `HV_X64_MSR_GUEST_OS_ID` (open source, OS type undefined: `0x8000000000000000`), then
  places the hypercall page on a zeroed frame of its own, mapped read and execute in kernel
  mode only in a kernel virtual area, by `HV_X64_MSR_HYPERCALL` with its reserved bits
  kept. The mapping and the frame are kept for as long as the machine runs; nothing calls
  the page yet (item 1.9c). A locked hypercall MSR, or a page that does not enable, is
  logged and the boot goes on.

Every Hyper-V constant is the vendored `hvdef`'s (`../vendor/openvmm/hvdef`, MIT), which
OSTD now depends on for the x86 kernel target. What is decided rather than done is in two
more added files that name nothing of OSTD: `src/arch/x86/cpu/hypervisor.rs` (the
hypervisor, the clock rates, the guest OS ID and the hypercall MSR's value, read through a
trait for the CPU) and `src/arch/x86/timer/pit/presence.rs` (the PIT's presence from its
counts, and the two lines).

This directory is also the crate `moruna-ostd-adaptation`, which compiles those four
added files for the host and tests them (`cargo test -p moruna-ostd-adaptation`): the
hypervisor and its clock rates over CPUID and MSR answers recorded for KVM, Hyper-V with
and without the frequency MSRs, bare metal with leaf 0x15, and a machine with nothing at
all. They carry `SPDX-License-Identifier: MPL-2.0`, as the files beside them do.

Three of its comments lose their em dashes (`src/log/logger.rs`, `src/log/macros.rs`,
`src/task/scheduler/mod.rs`), which the repository's quality check refuses in every
tracked file; their code is unchanged.

`upstream-files.json` records every file of the unmodified crate. `provenance.json`
records the archive's checksum, the changed files' checksums and the added files'. The
one dependency added is `hvdef`; the one frame allocation and kernel mapping added are
the hypercall page's, on Hyper-V. No metadata changes.
