//! The parts of a little-endian ELF64 file the usage build reads: sections by name, the
//! loadable segments (for an address's physical location and its bytes in the file) and the
//! symbol table. Any machine is read: the launcher's private copy of the kernel says EM_386
//! for QEMU's loader.

use alloc::{string::String, vec::Vec};
use core::fmt;

/// Why a file was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// `SHT_NOBITS`: a section with no bytes in the file.
pub const NOBITS: u32 = 8;
/// `SHT_SYMTAB`.
pub const SYMTAB: u32 = 2;
/// `PT_LOAD`.
pub const LOAD: u32 = 1;
/// `STT_FUNC`.
pub const FUNC: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub kind: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub value: u64,
    pub size: u64,
    /// `STT_*`, the low four bits of `st_info`.
    pub kind: u8,
}

/// A parsed file, borrowing its bytes.
pub struct Elf<'a> {
    data: &'a [u8],
    pub sections: Vec<Section>,
    pub segments: Vec<Segment>,
}

fn u16_at(d: &[u8], at: u64) -> Result<u16, Error> {
    let at = usize::try_from(at).map_err(|_| Error("offset out of range"))?;
    d.get(at..at.checked_add(2).ok_or(Error("offset out of range"))?)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(Error("truncated"))
}

fn u32_at(d: &[u8], at: u64) -> Result<u32, Error> {
    let at = usize::try_from(at).map_err(|_| Error("offset out of range"))?;
    d.get(at..at.checked_add(4).ok_or(Error("offset out of range"))?)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error("truncated"))
}

fn u64_at(d: &[u8], at: u64) -> Result<u64, Error> {
    let at = usize::try_from(at).map_err(|_| Error("offset out of range"))?;
    d.get(at..at.checked_add(8).ok_or(Error("offset out of range"))?)
        .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        .ok_or(Error("truncated"))
}

fn slice(d: &[u8], offset: u64, len: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    d.get(start..end)
}

/// The NUL-terminated string at `at` in a string table.
fn string(table: &[u8], at: u32) -> Result<String, Error> {
    let rest = table
        .get(at as usize..)
        .ok_or(Error("name out of its table"))?;
    let end = rest
        .iter()
        .position(|b| *b == 0)
        .ok_or(Error("unterminated name"))?;
    core::str::from_utf8(&rest[..end])
        .map(String::from)
        .map_err(|_| Error("name is not UTF-8"))
}

impl<'a> Elf<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Elf<'a>, Error> {
        if data.get(..4) != Some(b"\x7fELF".as_slice()) {
            return Err(Error("not an ELF file"));
        }
        if data.get(4) != Some(&2) || data.get(5) != Some(&1) {
            return Err(Error("not a little-endian ELF64 file"));
        }
        let phoff = u64_at(data, 0x20)?;
        let shoff = u64_at(data, 0x28)?;
        let phentsize = u64::from(u16_at(data, 0x36)?);
        let phnum = u64::from(u16_at(data, 0x38)?);
        let shentsize = u64::from(u16_at(data, 0x3a)?);
        let shnum = u64::from(u16_at(data, 0x3c)?);
        let shstrndx = u64::from(u16_at(data, 0x3e)?);
        if (phnum > 0 && phentsize < 56) || (shnum > 0 && shentsize < 64) {
            return Err(Error("header entries too small"));
        }
        let mut segments = Vec::new();
        for i in 0..phnum {
            let at = phoff + i * phentsize;
            segments.push(Segment {
                kind: u32_at(data, at)?,
                offset: u64_at(data, at + 8)?,
                vaddr: u64_at(data, at + 16)?,
                paddr: u64_at(data, at + 24)?,
                filesz: u64_at(data, at + 32)?,
                memsz: u64_at(data, at + 40)?,
            });
        }
        let mut raw = Vec::new();
        for i in 0..shnum {
            let at = shoff + i * shentsize;
            raw.push((
                u32_at(data, at)?,
                Section {
                    name: String::new(),
                    kind: u32_at(data, at + 4)?,
                    flags: u64_at(data, at + 8)?,
                    addr: u64_at(data, at + 16)?,
                    offset: u64_at(data, at + 24)?,
                    size: u64_at(data, at + 32)?,
                    link: u32_at(data, at + 40)?,
                },
            ));
        }
        let mut sections = Vec::new();
        if !raw.is_empty() {
            let names = raw
                .get(shstrndx as usize)
                .map(|(_, s)| s)
                .ok_or(Error("no section name table"))?;
            let table = slice(data, names.offset, names.size)
                .ok_or(Error("section names out of the file"))?;
            for (name, mut section) in raw {
                section.name = string(table, name)?;
                sections.push(section);
            }
        }
        Ok(Elf {
            data,
            sections,
            segments,
        })
    }

    /// The first section called `name`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// A section's bytes in the file; none for a section with no bytes there.
    pub fn contents(&self, section: &Section) -> Option<&'a [u8]> {
        if section.kind == NOBITS {
            return None;
        }
        slice(self.data, section.offset, section.size)
    }

    /// The loadable segment that holds `vaddr` in memory.
    fn segment(&self, vaddr: u64) -> Option<&Segment> {
        self.segments
            .iter()
            .find(|s| s.kind == LOAD && vaddr >= s.vaddr && vaddr - s.vaddr < s.memsz)
    }

    /// Where `len` bytes at `vaddr` are loaded in physical memory, if one loadable segment
    /// holds them all.
    pub fn paddr(&self, vaddr: u64, len: u64) -> Option<u64> {
        let s = self.segment(vaddr)?;
        let offset = vaddr - s.vaddr;
        (offset.checked_add(len)? <= s.memsz).then(|| s.paddr.wrapping_add(offset))
    }

    /// The `len` bytes at `vaddr` as the file initialises them.
    pub fn read(&self, vaddr: u64, len: u64) -> Option<&'a [u8]> {
        let s = self.segment(vaddr)?;
        let offset = vaddr - s.vaddr;
        if offset.checked_add(len)? > s.filesz {
            return None;
        }
        slice(self.data, s.offset.checked_add(offset)?, len)
    }

    /// Every symbol of the symbol table, none when the file has none.
    pub fn symbols(&self) -> Result<Vec<Symbol>, Error> {
        let Some(table) = self.sections.iter().find(|s| s.kind == SYMTAB) else {
            return Ok(Vec::new());
        };
        let names = self
            .sections
            .get(table.link as usize)
            .and_then(|s| self.contents(s))
            .ok_or(Error("symbol names out of the file"))?;
        let bytes = self
            .contents(table)
            .ok_or(Error("symbols out of the file"))?;
        let mut out = Vec::with_capacity(bytes.len() / 24);
        for entry in bytes.as_chunks::<24>().0 {
            out.push(Symbol {
                name: string(names, u32_at(entry, 0)?)?,
                kind: entry[4] & 0xf,
                value: u64_at(entry, 8)?,
                size: u64_at(entry, 16)?,
            });
        }
        Ok(out)
    }
}

/// Builds ELF files for tests (this crate's, and the report's with the feature `testing`).
#[cfg(any(test, feature = "testing"))]
pub mod build {
    use alloc::{vec, vec::Vec};

    pub struct Sec<'a> {
        pub name: &'a str,
        pub kind: u32,
        pub addr: u64,
        pub bytes: &'a [u8],
        /// For NOBITS, the size in memory.
        pub size: u64,
    }

    /// An ELF64 file of the given sections, one PT_LOAD per section that has an address
    /// (physical = virtual - `offset`), and a symbol table of `(name, value, size, kind)`.
    pub fn elf(sections: &[Sec<'_>], symbols: &[(&str, u64, u64, u8)], phys: u64) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        // Section bodies.
        let mut placed = Vec::new();
        for s in sections {
            while !out.len().is_multiple_of(8) {
                out.push(0);
            }
            placed.push(out.len() as u64);
            out.extend_from_slice(s.bytes);
        }
        // Symbol names and symbols.
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24];
        for (name, value, size, kind) in symbols {
            let at = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            let mut e = vec![0u8; 24];
            e[..4].copy_from_slice(&at.to_le_bytes());
            e[4] = *kind;
            e[8..16].copy_from_slice(&value.to_le_bytes());
            e[16..24].copy_from_slice(&size.to_le_bytes());
            symtab.extend(e);
        }
        let strtab_at = out.len() as u64;
        out.extend_from_slice(&strtab);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        let symtab_at = out.len() as u64;
        out.extend_from_slice(&symtab);
        // Section names.
        let mut shstr = vec![0u8];
        let mut name_at = Vec::new();
        for n in sections
            .iter()
            .map(|s| s.name)
            .chain([".strtab", ".symtab", ".shstrtab"])
        {
            name_at.push(shstr.len() as u32);
            shstr.extend_from_slice(n.as_bytes());
            shstr.push(0);
        }
        let shstr_at = out.len() as u64;
        out.extend_from_slice(&shstr);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        // Program headers.
        let phoff = out.len() as u64;
        let mut phnum = 0u16;
        for (s, at) in sections.iter().zip(&placed) {
            if s.addr == 0 {
                continue;
            }
            let mut p = vec![0u8; 56];
            p[..4].copy_from_slice(&1u32.to_le_bytes());
            p[8..16].copy_from_slice(&at.to_le_bytes());
            p[16..24].copy_from_slice(&s.addr.to_le_bytes());
            p[24..32].copy_from_slice(&s.addr.wrapping_sub(phys).to_le_bytes());
            let filesz = if s.kind == 8 { 0 } else { s.bytes.len() as u64 };
            let memsz = if s.kind == 8 {
                s.size
            } else {
                s.bytes.len() as u64
            };
            p[32..40].copy_from_slice(&filesz.to_le_bytes());
            p[40..48].copy_from_slice(&memsz.to_le_bytes());
            out.extend(p);
            phnum += 1;
        }
        // Section headers: null, the given ones, .strtab, .symtab, .shstrtab.
        let shoff = out.len() as u64;
        out.extend(vec![0u8; 64]);
        let header = |name: u32, kind: u32, addr: u64, offset: u64, size: u64, link: u32| {
            let mut h = vec![0u8; 64];
            h[..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[16..24].copy_from_slice(&addr.to_le_bytes());
            h[24..32].copy_from_slice(&offset.to_le_bytes());
            h[32..40].copy_from_slice(&size.to_le_bytes());
            h[40..44].copy_from_slice(&link.to_le_bytes());
            h
        };
        for (i, (s, at)) in sections.iter().zip(&placed).enumerate() {
            let size = if s.kind == 8 {
                s.size
            } else {
                s.bytes.len() as u64
            };
            out.extend(header(name_at[i], s.kind, s.addr, *at, size, 0));
        }
        let n = sections.len();
        let strtab_index = (n + 1) as u32;
        out.extend(header(name_at[n], 3, 0, strtab_at, strtab.len() as u64, 0));
        out.extend(header(
            name_at[n + 1],
            2,
            0,
            symtab_at,
            symtab.len() as u64,
            strtab_index,
        ));
        out.extend(header(
            name_at[n + 2],
            3,
            0,
            shstr_at,
            shstr.len() as u64,
            0,
        ));
        let count = (n + 4) as u16;
        out[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        out[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        out[0x38..0x3a].copy_from_slice(&phnum.to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&count.to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&(count - 1).to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use super::{build::*, *};

    #[test]
    fn sections_segments_and_symbols_are_read() {
        let file = elf(
            &[
                Sec {
                    name: ".text",
                    kind: 1,
                    addr: 0xffff_ffff_8800_0000,
                    bytes: &[0xc3; 16],
                    size: 0,
                },
                Sec {
                    name: ".bss",
                    kind: NOBITS,
                    addr: 0xffff_ffff_8810_0000,
                    bytes: &[],
                    size: 64,
                },
                Sec {
                    name: ".comment",
                    kind: 1,
                    addr: 0,
                    bytes: b"rustc",
                    size: 0,
                },
            ],
            &[("main", 0xffff_ffff_8800_0000, 16, FUNC)],
            0xffff_ffff_8000_0000,
        );
        let e = Elf::parse(&file).unwrap();
        let text = e.section(".text").unwrap().clone();
        assert_eq!(e.contents(&text), Some([0xc3u8; 16].as_slice()));
        assert_eq!(e.paddr(text.addr + 4, 12), Some(0x800_0004));
        assert_eq!(e.paddr(text.addr + 4, 13), None);
        assert_eq!(e.paddr(0x1000, 1), None);
        assert_eq!(e.read(text.addr + 8, 8), Some([0xc3u8; 8].as_slice()));
        assert_eq!(e.read(text.addr + 8, 9), None);
        let bss = e.section(".bss").unwrap();
        assert_eq!((e.contents(bss), bss.size), (None, 64));
        assert_eq!(e.paddr(bss.addr, 64), Some(0x810_0000));
        assert_eq!(e.read(bss.addr, 1), None);
        assert_eq!(e.section(".data"), None);
        let symbols = e.symbols().unwrap();
        assert_eq!(symbols.len(), 2);
        assert_eq!(
            symbols[1],
            Symbol {
                name: "main".into(),
                value: 0xffff_ffff_8800_0000,
                size: 16,
                kind: FUNC
            }
        );
        assert_eq!(Error("x").to_string(), "x");
    }

    #[test]
    fn a_file_that_is_not_elf64_le_is_refused() {
        assert_eq!(Elf::parse(b"MZ").err(), Some(Error("not an ELF file")));
        let mut file = elf(&[], &[], 0);
        file[4] = 1;
        assert_eq!(
            Elf::parse(&file).err(),
            Some(Error("not a little-endian ELF64 file"))
        );
        let file = elf(&[], &[], 0);
        assert_eq!(Elf::parse(&file[..0x30]).err(), Some(Error("truncated")));
        let mut small = file.clone();
        small[0x3a] = 8;
        assert_eq!(
            Elf::parse(&small).err(),
            Some(Error("header entries too small"))
        );
        let mut no_names = file.clone();
        no_names[0x3e] = 200;
        assert_eq!(
            Elf::parse(&no_names).err(),
            Some(Error("no section name table"))
        );
    }

    #[test]
    fn a_file_without_sections_or_symbols_is_empty() {
        let mut file = vec![0u8; 64];
        file[..6].copy_from_slice(b"\x7fELF\x02\x01");
        let e = Elf::parse(&file).unwrap();
        assert!(e.sections.is_empty() && e.segments.is_empty());
        assert_eq!(e.symbols().unwrap(), vec![]);
    }

    #[test]
    fn names_out_of_their_table_are_refused() {
        let table = b"\0abc\0bad\xff\0";
        assert_eq!(string(table, 1).unwrap(), "abc");
        assert_eq!(string(table, 5).err(), Some(Error("name is not UTF-8")));
        assert_eq!(
            string(table, 40).err(),
            Some(Error("name out of its table"))
        );
        assert_eq!(string(b"abc", 0).err(), Some(Error("unterminated name")));
        assert_eq!(
            u64_at(&[0; 4], u64::MAX).err(),
            Some(Error("offset out of range"))
        );
    }
}
