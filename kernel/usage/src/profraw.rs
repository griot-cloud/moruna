//! The LLVM raw profile (`.profraw`, format version 10, LLVM 19 to 22) of a program, rebuilt
//! from its ELF file and its counters.
//!
//! A program built with `-C instrument-coverage` carries its profile's parts in sections:
//! `__llvm_prf_data` (one 64-byte record per function), `__llvm_prf_cnts` (the counters),
//! `__llvm_prf_bits` (MC/DC bitmaps, absent here), `__llvm_prf_names` (the function names),
//! and the vtable sections (absent for Rust). Only the counters change while it runs. LLVM's
//! profiling runtime (minicov in OSTD's `coverage` feature) writes a header and those
//! sections in order; this writes the same bytes, taking every section but the counters from
//! the file, as `lprofWriteDataImpl` does with no binary ids and no value profile.

use alloc::{vec, vec::Vec};
use core::fmt;

use crate::elf::Elf;

/// `INSTR_PROF_RAW_MAGIC_64`: `\xfflprofr\x81`.
pub const MAGIC: u64 = (255 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'r' as u64) << 8)
    | 129;
/// The raw format this writes.
pub const VERSION: u64 = 10;
/// `VARIANT_MASK_BYTE_COVERAGE`: counters are one byte each.
pub const BYTE_COVERAGE: u64 = 1 << 60;
/// `IPVK_Last`: the last value-profile kind (`IPVK_VTableTarget`).
pub const VALUE_KIND_LAST: u64 = 2;
/// One `__llvm_profile_data` record of format 10.
pub const DATA_RECORD: u64 = 64;
/// One `VTableProfData` record.
pub const VTABLE_RECORD: u64 = 24;

/// The sections, by name.
pub const DATA: &str = "__llvm_prf_data";
pub const COUNTERS: &str = "__llvm_prf_cnts";
pub const BITMAP: &str = "__llvm_prf_bits";
pub const NAMES: &str = "__llvm_prf_names";
pub const VTABLES: &str = "__llvm_prf_vtab";
pub const VNAMES: &str = "__llvm_prf_vns";
/// The symbol the compiler sets to the format version and its variant flags.
pub const VERSION_SYMBOL: &str = "__llvm_profile_raw_version";

/// Why a profile was not built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file has no section of this name: it was not built with coverage.
    Missing(&'static str),
    /// A section's size is not whole records.
    Size(&'static str),
    /// The file asks for another raw format.
    Version(u64),
    /// The counters given are not the counter section's size.
    Counters { given: usize, section: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Missing(name) => write!(f, "no {name} section: not built with coverage"),
            Error::Size(name) => write!(f, "{name} is not whole records"),
            Error::Version(v) => write!(
                f,
                "raw profile version {} (flags {:#x}); this writes {VERSION}",
                v & 0xffff_ffff,
                v >> 32
            ),
            Error::Counters { given, section } => {
                write!(
                    f,
                    "{given} bytes of counters for a {section}-byte counter section"
                )
            }
        }
    }
}

/// Where a program's counters are and how large: what a reader of its memory reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counters {
    pub addr: u64,
    pub size: u64,
    /// One byte per counter (else eight).
    pub bytes: bool,
}

impl Counters {
    /// How many counters there are.
    pub fn count(&self) -> u64 {
        if self.bytes { self.size } else { self.size / 8 }
    }
}

fn padding(size: u64) -> u64 {
    7 & (8 - size % 8)
}

/// The version word: the version symbol's value, else the plain format version.
pub fn version(elf: &Elf<'_>) -> Result<u64, Error> {
    let symbols = elf.symbols().map_err(|_| Error::Missing(VERSION_SYMBOL))?;
    let version = symbols
        .iter()
        .find(|s| s.name == VERSION_SYMBOL)
        .and_then(|s| elf.read(s.value, 8))
        .map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
        .unwrap_or(VERSION);
    if version & 0xffff_ffff != VERSION {
        return Err(Error::Version(version));
    }
    Ok(version)
}

/// The program's counters: where they are in memory, their size and their width.
pub fn counters(elf: &Elf<'_>) -> Result<Counters, Error> {
    let section = elf.section(COUNTERS).ok_or(Error::Missing(COUNTERS))?;
    let bytes = version(elf)? & BYTE_COVERAGE != 0;
    if !bytes && section.size % 8 != 0 {
        return Err(Error::Size(COUNTERS));
    }
    Ok(Counters {
        addr: section.addr,
        size: section.size,
        bytes,
    })
}

/// The raw profile of `elf` with `counters`, the counter section's bytes as read from the
/// running program.
pub fn build(elf: &Elf<'_>, counters: &[u8]) -> Result<Vec<u8>, Error> {
    let version = version(elf)?;
    let section = |name: &'static str| -> Result<(u64, u64, Vec<u8>), Error> {
        match elf.section(name) {
            Some(s) => {
                let bytes = elf.contents(s).map(<[u8]>::to_vec).unwrap_or_default();
                Ok((s.addr, s.size, bytes))
            }
            None => Err(Error::Missing(name)),
        }
    };
    let absent = |name: &'static str| section(name).unwrap_or((0, 0, Vec::new()));
    let (data_addr, data_size, data) = section(DATA)?;
    let (cnts_addr, cnts_size, _) = section(COUNTERS)?;
    let (names_addr, names_size, names) = section(NAMES)?;
    let (bits_addr, bits_size, bits) = absent(BITMAP);
    let (_, vtab_size, vtab) = absent(VTABLES);
    let (_, vnames_size, vnames) = absent(VNAMES);
    if data_size % DATA_RECORD != 0 || data.len() as u64 != data_size {
        return Err(Error::Size(DATA));
    }
    if vtab_size % VTABLE_RECORD != 0 {
        return Err(Error::Size(VTABLES));
    }
    if counters.len() as u64 != cnts_size {
        return Err(Error::Counters {
            given: counters.len(),
            section: cnts_size,
        });
    }
    let num_counters = if version & BYTE_COVERAGE != 0 {
        cnts_size
    } else {
        cnts_size / 8
    };
    let num_data = data_size / DATA_RECORD;
    let (counters_delta, names_delta) = if num_data == 0 && names_size == 0 {
        (0, 0)
    } else {
        (cnts_addr.wrapping_sub(data_addr), names_addr)
    };
    let header = [
        MAGIC,
        version,
        0, // binary ids
        num_data,
        0, // padding before the counters
        num_counters,
        padding(cnts_size),
        bits_size,
        padding(bits_size),
        names_size,
        counters_delta,
        bits_addr.wrapping_sub(data_addr),
        names_delta,
        vtab_size / VTABLE_RECORD,
        vnames_size,
        VALUE_KIND_LAST,
    ];
    let mut out = Vec::new();
    for word in header {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let pad = |out: &mut Vec<u8>, size: u64| out.extend(vec![0u8; padding(size) as usize]);
    out.extend_from_slice(&data);
    out.extend_from_slice(counters);
    pad(&mut out, cnts_size);
    out.extend_from_slice(&bits);
    pad(&mut out, bits_size);
    out.extend_from_slice(&names);
    pad(&mut out, names_size);
    out.extend_from_slice(&vtab);
    pad(&mut out, vtab_size);
    out.extend_from_slice(&vnames);
    pad(&mut out, vnames_size);
    Ok(out)
}

/// The program's fingerprint (`dump::fingerprint`): its data section's size and first eight
/// bytes, as the file has them.
pub fn fingerprint(elf: &Elf<'_>) -> Option<alloc::string::String> {
    let data = elf.section(DATA)?;
    let first = elf.contents(data)?.get(..8)?;
    let mut buf = [0u8; crate::dump::FINGERPRINT_MAX];
    let first = u64::from_le_bytes(first.try_into().ok()?);
    Some(crate::dump::fingerprint(data.size, first, &mut buf).into())
}

/// Eight-byte counters as the bytes of their section.
pub fn to_bytes(counters: &[u64]) -> Vec<u8> {
    counters.iter().flat_map(|c| c.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use super::*;
    use crate::elf::build::{Sec, elf};

    const DATA_AT: u64 = 0xffff_ffff_8820_0000;
    const CNTS_AT: u64 = 0xffff_ffff_8830_0000;
    const NAMES_AT: u64 = 0xffff_ffff_8840_0000;
    const VERSION_AT: u64 = 0xffff_ffff_8850_0000;

    fn word(bytes: &[u8], i: usize) -> u64 {
        u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
    }

    fn program(version: Option<u64>, cnts: &[u8], names: &[u8]) -> Vec<u8> {
        let data = [7u8; 128];
        let v = version.unwrap_or(0).to_le_bytes();
        let mut sections = vec![
            Sec {
                name: DATA,
                kind: 1,
                addr: DATA_AT,
                bytes: &data,
                size: 0,
            },
            Sec {
                name: COUNTERS,
                kind: 1,
                addr: CNTS_AT,
                bytes: cnts,
                size: 0,
            },
            Sec {
                name: NAMES,
                kind: 1,
                addr: NAMES_AT,
                bytes: names,
                size: 0,
            },
        ];
        let mut symbols = vec![];
        if version.is_some() {
            sections.push(Sec {
                name: ".rodata",
                kind: 1,
                addr: VERSION_AT,
                bytes: &v,
                size: 0,
            });
            symbols.push((VERSION_SYMBOL, VERSION_AT, 8, 1));
        }
        elf(&sections, &symbols, 0xffff_ffff_8000_0000)
    }

    #[test]
    fn the_profile_is_the_header_then_the_sections() {
        let file = program(Some(VERSION), &[0; 24], b"names");
        let e = Elf::parse(&file).unwrap();
        let c = counters(&e).unwrap();
        assert_eq!(
            (c.addr, c.size, c.bytes, c.count()),
            (CNTS_AT, 24, false, 3)
        );
        let live = to_bytes(&[1, 0, 9]);
        assert_eq!(fingerprint(&e).as_deref(), Some("80.0707070707070707"));
        let p = build(&e, &live).unwrap();
        let expect_header = [
            MAGIC,
            VERSION,
            0,
            2,
            0,
            3,
            0,
            0,
            0,
            5,
            CNTS_AT - DATA_AT,
            0u64.wrapping_sub(DATA_AT),
            NAMES_AT,
            0,
            0,
            2,
        ];
        for (i, w) in expect_header.iter().enumerate() {
            assert_eq!(word(&p, i), *w, "header word {i}");
        }
        let body = &p[128..];
        assert_eq!(&body[..128], &[7u8; 128]);
        assert_eq!(&body[128..152], live.as_slice());
        assert_eq!(&body[152..157], b"names");
        assert_eq!(&body[157..], &[0, 0, 0]);
        assert_eq!(MAGIC.to_be_bytes(), *b"\xfflprofr\x81");
    }

    #[test]
    fn without_a_version_symbol_the_plain_version_is_written() {
        let file = program(None, &[0; 8], b"12345678");
        let e = Elf::parse(&file).unwrap();
        let p = build(&e, &[0; 8]).unwrap();
        assert_eq!(word(&p, 1), VERSION);
        assert_eq!(p.len(), 128 + 128 + 8 + 8);
    }

    #[test]
    fn byte_counters_are_counted_by_the_byte() {
        let file = program(Some(VERSION | BYTE_COVERAGE), &[0; 5], b"n");
        let e = Elf::parse(&file).unwrap();
        let c = counters(&e).unwrap();
        assert_eq!((c.bytes, c.count()), (true, 5));
        let p = build(&e, &[1; 5]).unwrap();
        assert_eq!(word(&p, 5), 5);
        assert_eq!(word(&p, 6), 3);
    }

    #[test]
    fn what_cannot_be_written_is_refused() {
        let file = program(Some(9), &[0; 8], b"n");
        let e = Elf::parse(&file).unwrap();
        assert_eq!(build(&e, &[0; 8]), Err(Error::Version(9)));
        assert_eq!(counters(&e), Err(Error::Version(9)));
        assert!(
            Error::Version(9 | 1 << 60)
                .to_string()
                .contains("version 9 (flags 0x10000000)")
        );

        let file = program(Some(VERSION), &[0; 8], b"n");
        let e = Elf::parse(&file).unwrap();
        let err = build(&e, &[0; 16]).unwrap_err();
        assert_eq!(
            err,
            Error::Counters {
                given: 16,
                section: 8
            }
        );
        assert_eq!(
            err.to_string(),
            "16 bytes of counters for a 8-byte counter section"
        );

        let file = program(Some(VERSION), &[0; 7], b"n");
        let e = Elf::parse(&file).unwrap();
        assert_eq!(counters(&e), Err(Error::Size(COUNTERS)));

        let plain = elf(
            &[Sec {
                name: ".text",
                kind: 1,
                addr: 1,
                bytes: &[0],
                size: 0,
            }],
            &[],
            0,
        );
        let e = Elf::parse(&plain).unwrap();
        assert_eq!(counters(&e), Err(Error::Missing(COUNTERS)));
        assert_eq!(build(&e, &[]), Err(Error::Missing(DATA)));
        assert_eq!(
            Error::Missing(COUNTERS).to_string(),
            "no __llvm_prf_cnts section: not built with coverage"
        );
        assert_eq!(
            Error::Size(DATA).to_string(),
            "__llvm_prf_data is not whole records"
        );
    }

    #[test]
    fn data_that_is_not_whole_records_is_refused() {
        let data = [0u8; 70];
        let file = elf(
            &[
                Sec {
                    name: DATA,
                    kind: 1,
                    addr: DATA_AT,
                    bytes: &data,
                    size: 0,
                },
                Sec {
                    name: COUNTERS,
                    kind: 1,
                    addr: CNTS_AT,
                    bytes: &[0; 8],
                    size: 0,
                },
                Sec {
                    name: NAMES,
                    kind: 1,
                    addr: NAMES_AT,
                    bytes: b"n",
                    size: 0,
                },
            ],
            &[],
            0,
        );
        let e = Elf::parse(&file).unwrap();
        assert_eq!(build(&e, &[0; 8]), Err(Error::Size(DATA)));
        let file = elf(
            &[
                Sec {
                    name: DATA,
                    kind: 1,
                    addr: DATA_AT,
                    bytes: &[0; 64],
                    size: 0,
                },
                Sec {
                    name: COUNTERS,
                    kind: 1,
                    addr: CNTS_AT,
                    bytes: &[0; 8],
                    size: 0,
                },
                Sec {
                    name: NAMES,
                    kind: 1,
                    addr: NAMES_AT,
                    bytes: b"n",
                    size: 0,
                },
                Sec {
                    name: VTABLES,
                    kind: 1,
                    addr: 0x10,
                    bytes: &[0; 10],
                    size: 0,
                },
            ],
            &[],
            0,
        );
        let e = Elf::parse(&file).unwrap();
        assert_eq!(build(&e, &[0; 8]), Err(Error::Size(VTABLES)));
    }

    #[test]
    fn an_empty_profile_has_no_deltas() {
        let file = elf(
            &[
                Sec {
                    name: DATA,
                    kind: 1,
                    addr: DATA_AT,
                    bytes: &[],
                    size: 0,
                },
                Sec {
                    name: COUNTERS,
                    kind: 1,
                    addr: CNTS_AT,
                    bytes: &[],
                    size: 0,
                },
                Sec {
                    name: NAMES,
                    kind: 1,
                    addr: NAMES_AT,
                    bytes: &[],
                    size: 0,
                },
            ],
            &[],
            0,
        );
        let e = Elf::parse(&file).unwrap();
        let p = build(&e, &[]).unwrap();
        assert_eq!((word(&p, 10), word(&p, 12)), (0, 0));
    }
}
