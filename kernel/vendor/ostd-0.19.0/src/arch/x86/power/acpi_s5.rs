// SPDX-License-Identifier: MPL-2.0

//! moruna: the part of the ACPI S5 power-off that is arithmetic (Moruna's system design,
//! section 1B). It names nothing of OSTD, so Moruna's host tests compile this same file
//! (`kernel/ostd-adaptation`).
//!
//! `\_S5` is a package whose first element is SLP_TYPa, the value the PM1a control register
//! takes to enter S5. Small kernels read it without an AML interpreter: they scan the DSDT
//! for the name `_S5_` and read the package that follows it.

/// SLP_EN, bit 13 of the PM1 control register: written with SLP_TYP, it enters the state.
const SLP_EN: u16 = 1 << 13;
/// SLP_TYP is bits 10 to 12 of the PM1 control register.
const SLP_TYP_SHIFT: u32 = 10;
/// The largest value those three bits hold.
const SLP_TYP_MAX: u64 = 7;

const NAME: &[u8; 4] = b"_S5_";
const NAME_OP: u8 = 0x08;
const ROOT_PREFIX: u8 = b'\\';
const PACKAGE_OP: u8 = 0x12;

/// SLP_TYPa of `\_S5`, read from a DSDT's AML (the table after its header).
///
/// `None` when the AML declares no `_S5_` package whose first element is an integer that
/// fits SLP_TYP. An `_S5_` that is not such a declaration (a reference to the name, say)
/// is passed over, and the scan goes on to the next.
pub(crate) fn slp_typa(aml: &[u8]) -> Option<u8> {
    let mut from = 0;
    while let Some(at) = find_name(aml, from) {
        if let Some(value) = declared_value(aml, at) {
            return Some(value);
        }
        from = at + 1;
    }
    None
}

/// The value that enters S5 when written to the PM1a control register: SLP_TYPa with
/// SLP_EN. `slp_typa` is what [`slp_typa`] returned.
pub(crate) fn pm1a_control(slp_typa: u8) -> u16 {
    (u16::from(slp_typa) << SLP_TYP_SHIFT) | SLP_EN
}

/// Where `_S5_` next appears at or after `from`.
fn find_name(aml: &[u8], from: usize) -> Option<usize> {
    aml.get(from..)?
        .windows(NAME.len())
        .position(|window| window == NAME)
        .map(|offset| from + offset)
}

/// The first element of the package declared under the `_S5_` at `at`, if this `_S5_` is
/// a declaration: `NameOp`, the name (after a root prefix or none), then `PackageOp`.
fn declared_value(aml: &[u8], at: usize) -> Option<u8> {
    let before = aml.get(..at)?;
    if !matches!(before, [.., NAME_OP] | [.., NAME_OP, ROOT_PREFIX]) {
        return None;
    }
    let package = aml.get(at + NAME.len()..)?;
    let (&op, package) = package.split_first()?;
    if op != PACKAGE_OP {
        return None;
    }
    // PkgLength: the two high bits of its first byte count the bytes that follow it. With
    // none, the low six bits are the length; otherwise the low four are its lowest bits and
    // each following byte gives eight more. The length counts from its own first byte.
    let (&lead, _) = package.split_first()?;
    let following = usize::from(lead >> 6);
    let mut length = if following == 0 {
        usize::from(lead & 0x3f)
    } else {
        usize::from(lead & 0x0f)
    };
    for index in 0..following {
        let byte = *package.get(1 + index)?;
        length |= usize::from(byte) << (4 + 8 * index);
    }
    // The package's bytes after its PkgLength: the element count, then the elements.
    let body = package.get(..length)?.get(1 + following..)?;
    let (&elements, body) = body.split_first()?;
    if elements == 0 {
        return None;
    }
    let value = integer(body)?;
    if value > SLP_TYP_MAX {
        return None;
    }
    Some(value as u8)
}

/// The AML integer `bytes` begins with: `ZeroOp`, `OneOp`, or a byte, word, double word or
/// quad word after its prefix.
fn integer(bytes: &[u8]) -> Option<u64> {
    let (&op, rest) = bytes.split_first()?;
    let width = match op {
        0x00 => return Some(0),
        0x01 => return Some(1),
        0x0a => 1,
        0x0b => 2,
        0x0c => 4,
        0x0e => 8,
        _ => return None,
    };
    let mut value = 0u64;
    for (index, &byte) in rest.get(..width)?.iter().enumerate() {
        value |= u64::from(byte) << (8 * index);
    }
    Some(value)
}
