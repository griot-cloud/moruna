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
