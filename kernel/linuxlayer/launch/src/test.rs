// SPDX-License-Identifier: MPL-2.0

use alloc::vec;

use super::*;

/// A static x86-64 ELF with a read-only info segment at `IMAGE_BASE` and a code segment,
/// as the personality's linker script makes.
fn elf(info: [u64; 3], code_at: u64, entry: u64) -> Vec<u8> {
    let mut b = vec![0u8; 0x3000];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // 64-bit
    b[5] = 1; // little endian
    b[6] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    b[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&entry.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes()); // program headers at 64
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&2u16.to_le_bytes());
    let ph = |b: &mut Vec<u8>, at: usize, flags: u32, off: u64, vaddr: u64, sz: u64| {
        b[at..at + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        b[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        b[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        b[at + 24..at + 32].copy_from_slice(&vaddr.to_le_bytes());
        b[at + 32..at + 40].copy_from_slice(&sz.to_le_bytes());
        b[at + 40..at + 48].copy_from_slice(&sz.to_le_bytes());
        b[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    };
    ph(&mut b, 64, 4, 0x1000, IMAGE_BASE, 0x1000);
    ph(&mut b, 120, 5, 0x2000, code_at, 0x1000);
    for (i, w) in info.iter().enumerate() {
        b[0x1000 + 8 * i..0x1000 + 8 * i + 8].copy_from_slice(&w.to_le_bytes());
    }
    b
}

const CODE: u64 = IMAGE_BASE + 0x1000;

#[test]
fn plans_the_personality() {
    let bytes = elf([CODE, CODE + 0x40, CODE + 0x40], CODE, CODE + 0x100);
    let plan = Plan::new(&bytes).unwrap();
    assert_eq!(plan.entry, CODE + 0x100);
    assert_eq!(plan.copy, CODE..CODE + 0x40);
    assert_eq!(plan.recovery, CODE + 0x40);
    assert_eq!(plan.segments.len(), 2);
    assert_eq!(plan.stack % STACK_SIZE, 0);
    assert!(plan.stack >= CODE + 0x1000 + STACK_SIZE);
    assert_eq!(plan.stack_pointer(), plan.stack + STACK_SIZE - STACK_ARGS);
    assert_eq!(
        segment_flags(&plan.segments[1]),
        moruna_abi::map::READ | moruna_abi::map::EXECUTE
    );
    assert_eq!(segment_flags(&plan.segments[0]), moruna_abi::map::READ);
}

#[test]
fn refuses_what_it_cannot_launch() {
    // Copy routines outside code.
    let bytes = elf([IMAGE_BASE, IMAGE_BASE + 8, CODE], CODE, CODE);
    assert_eq!(Plan::new(&bytes), Err(PlanError::BadInfo));
    // An empty range.
    let bytes = elf([CODE, CODE, CODE], CODE, CODE);
    assert_eq!(Plan::new(&bytes), Err(PlanError::BadInfo));
    // An image below the region: a Linux program's address.
    let mut bytes = elf([CODE, CODE + 8, CODE], CODE, CODE);
    bytes[64 + 16..64 + 24].copy_from_slice(&0x40_0000u64.to_le_bytes());
    bytes[64 + 24..64 + 32].copy_from_slice(&0x40_0000u64.to_le_bytes());
    assert!(matches!(Plan::new(&bytes), Err(PlanError::Elf(_))));
    // No table at the image base.
    let mut bytes = elf([CODE, CODE + 8, CODE], CODE, CODE);
    bytes[64 + 16..64 + 24].copy_from_slice(&(IMAGE_BASE + 0x10_0000).to_le_bytes());
    bytes[64 + 24..64 + 32].copy_from_slice(&(IMAGE_BASE + 0x10_0000).to_le_bytes());
    assert_eq!(Plan::new(&bytes), Err(PlanError::NoInfo));
    // Not an ELF.
    assert!(matches!(Plan::new(b"no"), Err(PlanError::Elf(_))));
    // Too large: a code segment at the region's end.
    let end = REGION_BASE + REGION_LEN - 0x1000;
    let bytes = elf([end, end + 8, end], end, end);
    assert_eq!(Plan::new(&bytes), Err(PlanError::TooLarge));
}

#[test]
fn a_command_round_trips() {
    use crate::command::{BadCommand, Command, decode, encode};
    let command = Command {
        path: b"/kc1/bin/hello".to_vec(),
        argv: vec![b"hello".to_vec(), b"a b \"c\"".to_vec()],
        env: vec![b"PATH=/bin".to_vec(), b"X=1=2".to_vec()],
    };
    let mut bytes = encode(&command).unwrap();
    assert_eq!(decode(&bytes), Some(command.clone()));
    // A memory object's trailing zeros change nothing.
    bytes.extend_from_slice(&[0; 64]);
    assert_eq!(decode(&bytes), Some(command.clone()));
    let bare = Command {
        path: b"/init".to_vec(),
        ..Command::default()
    };
    assert_eq!(decode(&encode(&bare).unwrap()), Some(bare));
    assert_eq!(
        encode(&Command {
            path: b"/a\0b".to_vec(),
            ..Command::default()
        }),
        Err(BadCommand)
    );
    assert_eq!(
        encode(&Command {
            path: b"/a".to_vec(),
            argv: vec![Vec::new()],
            env: Vec::new()
        }),
        Err(BadCommand)
    );
    assert_eq!(decode(b""), None);
    assert_eq!(decode(b"\0"), None);
    assert_eq!(decode(b"/a\0x\0"), None);
    // Zero padding ends both lists.
    assert_eq!(
        decode(b"/a\0x\0\0"),
        Some(Command {
            path: b"/a".to_vec(),
            argv: vec![b"x".to_vec()],
            env: Vec::new()
        })
    );
}

#[test]
fn a_report_round_trips_and_a_missing_one_reads_as_none() {
    use crate::report::{EXITED, LEN, REFUSED_MAX, Report, SIGNALED, decode, encode};
    let report = Report {
        how: SIGNALED,
        code: -9,
        served: 1234,
        refused: vec![(41, 2), (334, 1)],
    };
    let bytes = encode(&report);
    assert_eq!(bytes.len(), LEN);
    assert_eq!(decode(&bytes), Some(report.clone()));
    assert_eq!(decode(&[0u8; LEN]), None);
    assert_eq!(decode(&bytes[..16]), None);
    let many = Report {
        how: EXITED,
        refused: (0..REFUSED_MAX as u64 + 5).map(|n| (n, 1)).collect(),
        ..Report::default()
    };
    assert_eq!(decode(&encode(&many)).unwrap().refused.len(), REFUSED_MAX);
    let mut lying = encode(&report);
    lying[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode(&lying), None);
}

#[test]
fn the_personality_s_slots_are_apart_from_the_program_s() {
    use crate::slot::*;
    let own = [
        SELF, CONSOLE, INITRAMFS, CMDLINE, FILESYSTEM, SCRATCH, INPUT, OUTPUT, LOG, PROCESSES,
        COMMAND, REPORT,
    ];
    for name in own {
        assert!(name.starts_with(PREFIX) && !is_program(name) && !program_name_ok(name));
    }
    assert!(program_name_ok("vsock") && program_name_ok("filesystem"));
    assert!(program_name_ok(&"x".repeat(32)));
    assert!(!program_name_ok(&"x".repeat(33)));
    assert!(!program_name_ok(""));
    assert!(!program_name_ok("lx.anything"));
}

#[test]
fn a_program_s_start_page_lays_out_as_the_core_s() {
    use moruna_abi::{ABI_VERSION, PAGE_SIZE, limits, read_u32, start_page as page};

    use crate::program_page::{Entry, encode};
    let entries = [
        Entry {
            handle: 7,
            rights: 0x13,
            name: "vsock",
        },
        Entry {
            handle: 9,
            rights: 0x01,
            name: "filesystem",
        },
    ];
    let p = encode("lx-moruna-engine", 4, &entries).unwrap();
    assert_eq!(p.len(), PAGE_SIZE as usize);
    assert_eq!(read_u32(&p, page::MAGIC_OFFSET), Some(page::MAGIC));
    assert_eq!(read_u32(&p, page::VERSION_OFFSET), Some(ABI_VERSION as u32));
    assert_eq!(read_u32(&p, page::COUNT_OFFSET), Some(2));
    assert_eq!(read_u32(&p, page::CPUS_OFFSET), Some(4));
    assert_eq!(&p[page::NAME_OFFSET..][..16], b"lx-moruna-engine");
    assert_eq!(p[page::NAME_OFFSET + 16], 0);
    let second = page::HEADER_SIZE + page::ENTRY_SIZE;
    assert_eq!(read_u32(&p, second + page::ENTRY_HANDLE), Some(9));
    assert_eq!(read_u32(&p, second + page::ENTRY_RIGHTS), Some(1));
    assert_eq!(&p[second + page::ENTRY_NAME..][..11], b"filesystem\0");
    // A long process name is cut; too many entries or a long name refuse.
    let long = "p".repeat(40);
    let p = encode(&long, 1, &[]).unwrap();
    assert_eq!(
        &p[page::NAME_OFFSET..][..limits::NAME_MAX],
        &long.as_bytes()[..32]
    );
    let name = "n".repeat(33);
    let bad = Entry {
        handle: 1,
        rights: 0,
        name: &name,
    };
    assert!(encode("x", 1, &[bad]).is_none());
    let one = Entry {
        handle: 1,
        rights: 0,
        name: "a",
    };
    assert!(encode("x", 1, &[one; limits::START_HANDLES_MAX]).is_some());
    assert!(encode("x", 1, &[one; limits::START_HANDLES_MAX + 1]).is_none());
}

#[test]
fn the_contract_s_values_copy_compare_and_print() {
    use alloc::format;

    let bytes = elf([CODE, CODE + 0x40, CODE + 0x40], CODE, CODE + 0x100);
    let plan = Plan::new(&bytes).unwrap();
    assert_eq!(plan.clone(), plan);
    assert!(format!("{plan:?}").contains("recovery"));
    let error = PlanError::TooLarge;
    assert_eq!(error, error.clone());
    assert!(format!("{error:?} {:?}", PlanError::NoInfo).contains("TooLarge"));
    // A writable segment maps writable.
    let mut writable = plan.segments[0];
    writable.write = true;
    assert_eq!(
        segment_flags(&writable),
        moruna_abi::map::READ | moruna_abi::map::WRITE
    );
    let report = report::Report {
        how: report::SIGNALED,
        code: 9,
        served: 1,
        refused: vec![(41, 2)],
    };
    assert_eq!(report.clone(), report);
    assert!(format!("{report:?}").contains("refused"));
    let command = command::Command::default();
    assert_eq!(command.clone(), command);
    assert!(format!("{command:?}").contains("argv"));
    let bad = command::BadCommand;
    assert_eq!(bad, bad.clone());
    assert_eq!(format!("{bad:?}"), "BadCommand");
    let entry = program_page::Entry {
        handle: 1,
        rights: 2,
        name: "n",
    };
    let copied = entry;
    assert!(format!("{copied:?}").contains("rights"));
}
