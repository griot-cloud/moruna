//! The ELF loader's plan (SDD 28, f.2): what a Linux program's headers say to map, as
//! Linux's own loader reads them. `ET_EXEC` is mapped where it says; `ET_DYN` (a
//! position-independent program, or the interpreter) at a base the caller chooses.
//! `PT_INTERP` names the program that is started instead, with the program mapped
//! beside it (`ld-linux-x86-64.so.2`, `ld-musl-x86_64.so.1`).

use alloc::vec::Vec;

pub const PAGE: u64 = 4096;
const EHDR: usize = 64;
const PHDR: usize = 56;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
/// The most program headers read.
pub const MAX_PHDRS: usize = 128;
/// The longest interpreter path.
pub const INTERP_MAX: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    NotElf,
    NotSupported,
    BadProgramHeaders,
    BadSegment,
    NoSegments,
    BadInterpreter,
}

impl ElfError {
    pub fn describe(self) -> &'static str {
        match self {
            ElfError::Truncated => "the headers end early",
            ElfError::NotElf => "not an ELF file",
            ElfError::NotSupported => {
                "not a 64-bit little-endian x86-64 executable or shared object"
            }
            ElfError::BadProgramHeaders => "the program headers are malformed or too many",
            ElfError::BadSegment => "a segment's sizes, offsets or alignment are inconsistent",
            ElfError::NoSegments => "nothing to load",
            ElfError::BadInterpreter => "the interpreter's path is malformed",
        }
    }
}

/// A loadable segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub offset: u64,
    pub filesz: u64,
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Segment {
    /// The first page the segment touches, before the base is added.
    pub fn page_start(&self) -> u64 {
        self.vaddr & !(PAGE - 1)
    }

    /// One past the last page it touches.
    pub fn page_end(&self) -> u64 {
        (self.vaddr + self.memsz).div_ceil(PAGE) * PAGE
    }

    /// The file offset of its first page.
    pub fn file_page(&self) -> u64 {
        self.offset & !(PAGE - 1)
    }

    /// The bytes of the file its pages hold, from `file_page`: up to its last file
    /// byte.
    pub fn file_span(&self) -> u64 {
        (self.offset & (PAGE - 1)) + self.filesz
    }
}

/// What the headers say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// `ET_DYN`: mapped at a base the loader picks.
    pub relocatable: bool,
    pub entry: u64,
    /// In file order.
    pub segments: Vec<Segment>,
    /// Where the interpreter's path is in the file, and its length with its NUL.
    pub interp: Option<(u64, u64)>,
    /// Where the program headers are once mapped, before the base is added.
    pub phdr: u64,
    pub phnum: u64,
}

impl Plan {
    /// The pages the image spans, from its lowest page to past its highest.
    pub fn span(&self) -> (u64, u64) {
        let low = self
            .segments
            .iter()
            .map(|s| s.page_start())
            .min()
            .unwrap_or(0);
        let high = self
            .segments
            .iter()
            .map(|s| s.page_end())
            .max()
            .unwrap_or(0);
        (low, high)
    }
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, ElfError> {
    let s = b.get(o..o + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, ElfError> {
    let s = b.get(o..o + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], o: usize) -> Result<u64, ElfError> {
    let s = b.get(o..o + 8).ok_or(ElfError::Truncated)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

/// Where the program headers are in the file: `(offset, length)`, from the first 64
/// bytes. The caller reads them and gives them to [`parse`].
pub fn headers(ehdr: &[u8]) -> Result<(u64, u64), ElfError> {
    let ident = ehdr.get(..16).ok_or(ElfError::Truncated)?;
    if ident[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::NotElf);
    }
    if ident[4] != 2 || ident[5] != 1 {
        return Err(ElfError::NotSupported);
    }
    if ehdr.len() < EHDR {
        return Err(ElfError::Truncated);
    }
    let kind = u16_at(ehdr, 16)?;
    if !(kind == 2 || kind == 3) || u16_at(ehdr, 18)? != 62 {
        return Err(ElfError::NotSupported);
    }
    let phoff = u64_at(ehdr, 32)?;
    let phentsize = u16_at(ehdr, 54)? as usize;
    let phnum = u16_at(ehdr, 56)? as usize;
    if phentsize != PHDR || phnum == 0 || phnum > MAX_PHDRS {
        return Err(ElfError::BadProgramHeaders);
    }
    Ok((phoff, (phnum * PHDR) as u64))
}

/// Parses the ELF header `ehdr` and the program headers `phdrs` (read from where
/// [`headers`] said), for a file of `file_size` bytes.
pub fn parse(ehdr: &[u8], phdrs: &[u8], file_size: u64) -> Result<Plan, ElfError> {
    let (phoff, len) = headers(ehdr)?;
    if phdrs.len() as u64 != len {
        return Err(ElfError::BadProgramHeaders);
    }
    let relocatable = u16_at(ehdr, 16)? == 3;
    let entry = u64_at(ehdr, 24)?;
    let mut segments = Vec::new();
    let mut interp = None;
    let mut phdr = None;
    for i in 0..phdrs.len() / PHDR {
        let at = i * PHDR;
        let kind = u32_at(phdrs, at)?;
        let flags = u32_at(phdrs, at + 4)?;
        let offset = u64_at(phdrs, at + 8)?;
        let vaddr = u64_at(phdrs, at + 16)?;
        let filesz = u64_at(phdrs, at + 32)?;
        let memsz = u64_at(phdrs, at + 40)?;
        match kind {
            PT_LOAD => {
                if memsz == 0 {
                    continue;
                }
                let file_end = offset.checked_add(filesz).ok_or(ElfError::BadSegment)?;
                let mem_end = vaddr.checked_add(memsz).ok_or(ElfError::BadSegment)?;
                if filesz > memsz
                    || file_end > file_size
                    || mem_end > 0x0000_7fff_ffff_f000
                    || offset % PAGE != vaddr % PAGE
                {
                    return Err(ElfError::BadSegment);
                }
                segments.push(Segment {
                    vaddr,
                    memsz,
                    offset,
                    filesz,
                    read: flags & 4 != 0,
                    write: flags & 2 != 0,
                    execute: flags & 1 != 0,
                });
            }
            PT_INTERP => {
                if !(2..=INTERP_MAX).contains(&filesz)
                    || offset.checked_add(filesz).is_none_or(|e| e > file_size)
                {
                    return Err(ElfError::BadInterpreter);
                }
                interp = Some((offset, filesz));
            }
            PT_PHDR => phdr = Some(vaddr),
            _ => {}
        }
    }
    if segments.is_empty() {
        return Err(ElfError::NoSegments);
    }
    // Without PT_PHDR the headers are wherever the segment holding their offset maps.
    let phdr = match phdr {
        Some(v) => v,
        None => segments
            .iter()
            .find(|s| s.offset <= phoff && phoff + len <= s.offset + s.filesz)
            .map(|s| s.vaddr + (phoff - s.offset))
            .unwrap_or(0),
    };
    Ok(Plan {
        relocatable,
        entry,
        segments,
        interp,
        phdr,
        phnum: len / PHDR as u64,
    })
}

/// The interpreter's path from its `PT_INTERP` bytes: up to the NUL, which must end it.
pub fn interpreter(bytes: &[u8]) -> Result<&[u8], ElfError> {
    match bytes.split_last() {
        Some((0, path)) if !path.is_empty() && !path.contains(&0) && path[0] == b'/' => Ok(path),
        _ => Err(ElfError::BadInterpreter),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// An ELF header and program headers: `segs` is `(type, flags, offset, vaddr,
    /// filesz, memsz)`.
    pub fn image(kind: u16, entry: u64, segs: &[(u32, u32, u64, u64, u64, u64)]) -> Vec<u8> {
        let mut b = alloc::vec![0u8; EHDR + segs.len() * PHDR];
        b[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&kind.to_le_bytes());
        b[18..20].copy_from_slice(&62u16.to_le_bytes());
        b[24..32].copy_from_slice(&entry.to_le_bytes());
        b[32..40].copy_from_slice(&(EHDR as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes());
        b[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
        for (i, s) in segs.iter().enumerate() {
            let at = EHDR + i * PHDR;
            b[at..at + 4].copy_from_slice(&s.0.to_le_bytes());
            b[at + 4..at + 8].copy_from_slice(&s.1.to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&s.2.to_le_bytes());
            b[at + 16..at + 24].copy_from_slice(&s.3.to_le_bytes());
            b[at + 32..at + 40].copy_from_slice(&s.4.to_le_bytes());
            b[at + 40..at + 48].copy_from_slice(&s.5.to_le_bytes());
        }
        b
    }

    fn plan(b: &[u8], size: u64) -> Result<Plan, ElfError> {
        let (off, len) = headers(&b[..EHDR])?;
        parse(&b[..EHDR], &b[off as usize..(off + len) as usize], size)
    }

    #[test]
    fn lx_t3_static_and_dynamic() {
        let b = image(
            2,
            0x401000,
            &[
                (1, 5, 0, 0x400000, 0x2000, 0x2000),
                (1, 6, 0x2000, 0x402000, 0x100, 0x3000),
            ],
        );
        let p = plan(&b, 0x3000).unwrap();
        assert!(!p.relocatable);
        assert_eq!(p.entry, 0x401000);
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.interp, None);
        assert_eq!(p.phdr, 0x400000 + EHDR as u64);
        assert_eq!(p.phnum, 2);
        assert_eq!(p.span(), (0x400000, 0x405000));
        let s = p.segments[1];
        assert!(s.write && s.read && !s.execute);
        assert_eq!(
            (s.page_start(), s.page_end(), s.file_page(), s.file_span()),
            (0x402000, 0x405000, 0x2000, 0x100)
        );

        let d = image(
            3,
            0x1100,
            &[
                (6, 4, 64, 64, 112, 112),
                (3, 4, 0x200, 0x200, 28, 28),
                (1, 5, 0, 0, 0x1200, 0x1200),
                (1, 6, 0x1f00, 0x2f00, 0x10, 0x20),
            ],
        );
        let p = plan(&d, 0x2000).unwrap();
        assert!(p.relocatable);
        assert_eq!(p.interp, Some((0x200, 28)));
        assert_eq!(p.phdr, 64);
        assert_eq!(p.segments[1].file_span(), 0xf10);
    }

    #[test]
    fn lx_t3_refusals() {
        assert_eq!(headers(&[0u8; 8]), Err(ElfError::Truncated));
        assert_eq!(headers(&[0u8; 64]), Err(ElfError::NotElf));
        let mut b = image(2, 0, &[(1, 5, 0, 0x400000, 16, 16)]);
        b[4] = 1;
        assert_eq!(headers(&b), Err(ElfError::NotSupported));
        let mut b = image(1, 0, &[(1, 5, 0, 0x400000, 16, 16)]);
        assert_eq!(headers(&b), Err(ElfError::NotSupported));
        b[16] = 2;
        b[18] = 3;
        assert_eq!(headers(&b), Err(ElfError::NotSupported));
        let b = image(2, 0, &[]);
        assert_eq!(headers(&b), Err(ElfError::BadProgramHeaders));
        let b = image(2, 0, &[(1, 5, 0, 0x400000, 0x20, 0x10)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::BadSegment));
        let b = image(2, 0, &[(1, 5, 0, 0x400000, 0x2000, 0x2000)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::BadSegment));
        let b = image(2, 0, &[(1, 5, 0x10, 0x400000, 0x10, 0x10)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::BadSegment));
        let b = image(2, 0, &[(1, 5, 0, 0x7fff_ffff_f000, 0x10, 0x2000)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::BadSegment));
        let b = image(2, 0, &[(1, 5, 0, 0x400000, 0, 0)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::NoSegments));
        let b = image(2, 0, &[(3, 4, 0x100, 0, 1, 1), (1, 5, 0, 0x400000, 16, 16)]);
        assert_eq!(plan(&b, 0x1000), Err(ElfError::BadInterpreter));
        let b = image(2, 0, &[(1, 5, 0, 0x400000, 16, 16)]);
        let (off, len) = headers(&b).unwrap();
        assert_eq!(
            parse(
                &b[..EHDR],
                &b[off as usize..(off + len - 1) as usize],
                0x1000
            ),
            Err(ElfError::BadProgramHeaders)
        );
        for e in [
            ElfError::Truncated,
            ElfError::NotElf,
            ElfError::NotSupported,
            ElfError::BadProgramHeaders,
            ElfError::BadSegment,
            ElfError::NoSegments,
            ElfError::BadInterpreter,
        ] {
            assert!(!e.describe().is_empty());
        }
        // Headers no loaded segment holds, and no PT_PHDR: no address for them.
        let b = image(2, 0, &[(1, 5, 0x1000, 0x401000, 16, 16)]);
        assert_eq!(plan(&b, 0x2000).unwrap().phdr, 0);
    }

    #[test]
    fn lx_t3_interpreter_path() {
        assert_eq!(interpreter(b"/lib/ld.so\0"), Ok(&b"/lib/ld.so"[..]));
        assert_eq!(interpreter(b"/lib/ld.so"), Err(ElfError::BadInterpreter));
        assert_eq!(interpreter(b"lib\0"), Err(ElfError::BadInterpreter));
        assert_eq!(interpreter(b"/a\0b\0"), Err(ElfError::BadInterpreter));
        assert_eq!(interpreter(b"\0"), Err(ElfError::BadInterpreter));
    }
}
