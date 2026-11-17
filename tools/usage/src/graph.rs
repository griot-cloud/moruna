//! What kernel-mode code a user program can reach: the direct calls and jumps in the kernel's
//! disassembly, followed from the user-mode loop. A call through a pointer (a `dyn` method, a
//! function pointer) is not followed, so this finds a lower bound: a function not reached may
//! still be reachable that way, and the report says so.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::llvm::demangle;

/// Where a user program enters kernel mode: the loop that runs a thread in user mode and takes
/// its calls, exceptions and faults back (`kernel/core/src/process.rs`'s `run`, which the
/// compiler inlines into the thread's closure), OSTD's user-mode entry and user page fault, the
/// native dispatch and the Linux fast path. A function whose name holds one of these is an
/// entry.
pub const USER_ENTRIES: [&str; 7] = [
    "moruna_microkernel::process::run",
    "<moruna_microkernel::process::Thread>::spawn_with::{closure#0} as core::ops::function::FnOnce",
    "UserContextApiInternal>::execute",
    "ostd::mm::fault::handle_user_page_fault",
    "<moruna_microkernel::vm::Space>::fault",
    "moruna_microkernel::syscall::dispatch",
    "moruna_microkernel::foreign::call",
];

/// Function name (demangled, no hashes) to the functions it calls or jumps to directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    pub edges: BTreeMap<String, BTreeSet<String>>,
}

/// The symbol in `<NAME>` or `<NAME+0x10>` at the end of an instruction line.
fn target(line: &str) -> Option<&str> {
    let open = line.rfind('<')?;
    let inner = line[open + 1..].strip_suffix('>')?;
    Some(match inner.rfind("+0x") {
        Some(at) => &inner[..at],
        None => inner,
    })
}

impl Graph {
    /// The graph of `llvm-objdump -d --no-show-raw-insn` output.
    pub fn parse(disassembly: &str) -> Graph {
        let mut graph = Graph::default();
        let mut current: Option<String> = None;
        for line in disassembly.lines() {
            let line = line.trim_end();
            if line.ends_with(">:") && !line.starts_with(' ') && !line.contains('\t') {
                current = target(&line[..line.len() - 1]).map(demangle);
                if let Some(name) = &current {
                    graph.edges.entry(name.clone()).or_default();
                }
                continue;
            }
            let Some(from) = &current else { continue };
            let Some(op) = line.split('\t').nth(1) else {
                continue;
            };
            let op = op.trim();
            if !(op.starts_with("call") || op.starts_with('j')) {
                continue;
            }
            if let Some(to) = target(line).map(demangle)
                && &to != from
            {
                graph.edges.entry(from.clone()).or_default().insert(to);
            }
        }
        graph
    }

    /// Every function reachable from a function whose name holds one of `roots`.
    pub fn reachable(&self, roots: &[&str]) -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = self
            .edges
            .keys()
            .filter(|n| roots.iter().any(|r| n.contains(r)))
            .cloned()
            .collect();
        let mut queue: VecDeque<String> = seen.iter().cloned().collect();
        while let Some(name) = queue.pop_front() {
            for next in self.edges.get(&name).into_iter().flatten() {
                if seen.insert(next.clone()) {
                    queue.push_back(next.clone());
                }
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISASSEMBLY: &str = "
/kernel.elf:\tfile format elf64-x86-64

Disassembly of section .text:

ffffffff8804b000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel7process3run>:
ffffffff8804b000:      \tpushq\t%rbp
ffffffff8804b004:      \tcallq\t0xffffffff88100000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel7syscall8dispatch>
ffffffff8804b009:      \tcallq\t*%rax
ffffffff8804b00b:      \tjmp\t0xffffffff8804b000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel7process3run+0x0>

ffffffff88100000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel7syscall8dispatch>:
ffffffff88100000:      \tjmp\t0xffffffff88200010 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel6memory3pin+0x10>
ffffffff88100005:      \tmovq\t0x10(%rip), %rax  # 0xffffffff88300000 <_RNvCs1_4ostd4boot>

ffffffff88200000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel6memory3pin>:
ffffffff88200000:      \tretq

ffffffff88300000 <_RNvCs1_4ostd4boot>:
ffffffff88300000:      \tcallq\t0xffffffff88200000 <_RNvNtCs7n3AVLcjyHa_18moruna_microkernel6memory3pin>
";

    #[test]
    fn direct_calls_and_jumps_are_followed_from_the_user_entries() {
        let g = Graph::parse(DISASSEMBLY);
        assert_eq!(g.edges.len(), 4);
        assert_eq!(
            g.edges["moruna_microkernel::process::run"],
            BTreeSet::from(["moruna_microkernel::syscall::dispatch".to_string()])
        );
        let reached = g.reachable(&USER_ENTRIES);
        assert!(reached.contains("moruna_microkernel::memory::pin"));
        assert!(!reached.contains("ostd::boot"), "a load is not a call");
        assert_eq!(g.reachable(&["nothing"]), BTreeSet::new());
        assert_eq!(target("no symbol"), None);
    }
}
