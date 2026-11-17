// SPDX-License-Identifier: MPL-2.0

//! This module offers `/proc/cpuinfo` file support, which provides
//! information about the CPU architecture, cores, and other details.
//!
//! Reference: <https://man7.org/linux/man-pages/man5/proc_cpuinfo.5.html>

use alloc::format;

use aster_util::printer::VmPrinter;
use ostd::{
    cpu::{PinCurrentCpu, all_cpus},
    cpu_local,
    task::disable_preempt,
};
use spin::Once;

use crate::{
    arch::cpu::CpuInformation,
    fs::{
        file::mkmod,
        procfs::template::{ProcFile, ProcFileOps},
        vfs::inode::Inode,
    },
    prelude::*,
};

/// Represents the inode at `/proc/cpuinfo`.
pub(super) struct CpuInfoFileOps;

impl CpuInfoFileOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference:
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/cpuinfo.c#L25>
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/generic.c#L549-L550>
        ProcFile::new(Self, parent, mkmod!(a+r))
    }
}

impl ProcFileOps for CpuInfoFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);

        // In the personality only the first thread's CPU is probed (every thread is on CPU 0
        // for per-CPU state), so a CPU without its own record is told as the first one is.
        let first = all_cpus().find_map(|cpu| CPU_INFORMATION.get_on_cpu(cpu).get());
        for cpu in all_cpus() {
            let cpu_info = match CPU_INFORMATION.get_on_cpu(cpu).get() {
                Some(info) => info.to_string(),
                None => match first {
                    Some(info) => renumbered(&info.to_string(), u32::from(cpu) as usize),
                    None => continue,
                },
            };
            writeln!(printer, "{}", cpu_info)?;
        }

        Ok(printer.bytes_written())
    }
}

/// `info` with its first line, `processor : N`, saying `cpu` instead.
fn renumbered(info: &str, cpu: usize) -> String {
    match info.split_once('\n') {
        Some((_, rest)) => format!("processor\t: {}\n{}", cpu, rest),
        None => info.to_string(),
    }
}

cpu_local! {
    static CPU_INFORMATION: Once<CpuInformation> = Once::new();
}

pub(super) fn init_on_each_cpu() {
    let guard = disable_preempt();
    CPU_INFORMATION
        .get_on_cpu(guard.current_cpu())
        .call_once(|| CpuInformation::new(&guard));
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::renumbered;

    #[ktest]
    fn a_cpu_without_its_own_record_is_told_as_the_first() {
        let first = "processor\t: 0\nvendor_id\t: GenuineIntel\n";
        assert_eq!(
            renumbered(first, 3),
            "processor\t: 3\nvendor_id\t: GenuineIntel\n"
        );
        assert_eq!(renumbered("one line", 1), "one line");
    }
}
