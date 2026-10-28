//! Static ELF64 executables for x86_64 (SDD 20, f.2).
//!
//! The kernel loads only what it can load without a dynamic linker: `ET_EXEC`, little
//! endian, `EM_X86_64`, `PT_LOAD` segments on their own pages, no interpreter, no
//! dynamic section, no thread-local storage, and no segment both writable and
//! executable. Anything else is refused by name, never loaded partly.

use alloc::vec::Vec;

/// The page size segments are placed on.
pub const PAGE: u64 = 4096;
/// The most program headers an image may have.
pub const MAX_PHDRS: usize = 64;

const EHDR: usize = 64;
const PHDR: usize = 56;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// Why an image was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    NotElf,
    Not64Bit,
    NotLittleEndian,
    NotExecutable,
    WrongMachine,
    BadProgramHeaders,
    Dynamic,
    ThreadLocal,
    NoSegments,
    /// A segment's file bytes are larger than its memory size, or reach past the file.
    BadSegment,
    /// A segment lies outside the range the caller allows.
    OutOfRange,
    /// Two segments share a page.
    Overlap,
    /// A segment is both writable and executable.
    WriteExecute,
    /// The entry point is not in an executable segment.
    BadEntry,
}

impl ElfError {
    pub fn describe(self) -> &'static str {
        match self {
            ElfError::Truncated => "the image ends inside a header",
            ElfError::NotElf => "not an ELF image",
            ElfError::Not64Bit => "not a 64-bit ELF image",
            ElfError::NotLittleEndian => "not a little-endian ELF image",
            ElfError::NotExecutable => "not a static executable (ET_EXEC)",
            ElfError::WrongMachine => "not an x86_64 image",
            ElfError::BadProgramHeaders => "the program headers are malformed or too many",
            ElfError::Dynamic => "the image needs a dynamic linker",
            ElfError::ThreadLocal => "the image declares thread-local storage",
            ElfError::NoSegments => "the image has no loadable segment",
            ElfError::BadSegment => "a segment's sizes or offsets are inconsistent",
            ElfError::OutOfRange => "a segment lies outside the allowed address range",
            ElfError::Overlap => "two segments share a page",
            ElfError::WriteExecute => "a segment is both writable and executable",
            ElfError::BadEntry => "the entry point is not in an executable segment",
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
    /// The first page address the segment touches.
    pub fn page_start(&self) -> u64 {
        self.vaddr & !(PAGE - 1)
    }

    /// One past the last page address the segment touches.
    pub fn page_end(&self) -> u64 {
        // `parse` checked that `vaddr + memsz` rounded up does not overflow.
        (self.vaddr + self.memsz + PAGE - 1) & !(PAGE - 1)
    }
}

/// A parsed image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub entry: u64,
    /// In ascending address order.
    pub segments: Vec<Segment>,
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

/// Parses `bytes` and checks every segment lies in `allowed` (page aligned bounds).
pub fn parse(bytes: &[u8], allowed: core::ops::Range<u64>) -> Result<Image, ElfError> {
    let ident = bytes.get(..16).ok_or(ElfError::Truncated)?;
    if ident[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::NotElf);
    }
    if ident[4] != 2 {
        return Err(ElfError::Not64Bit);
    }
    if ident[5] != 1 {
        return Err(ElfError::NotLittleEndian);
    }
    if bytes.len() < EHDR {
        return Err(ElfError::Truncated);
    }
    if u16_at(bytes, 16)? != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if u16_at(bytes, 18)? != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }
    let entry = u64_at(bytes, 24)?;
    let phoff = usize::try_from(u64_at(bytes, 32)?).map_err(|_| ElfError::BadProgramHeaders)?;
    let phentsize = u16_at(bytes, 54)? as usize;
    let phnum = u16_at(bytes, 56)? as usize;
    if phentsize != PHDR || phnum > MAX_PHDRS {
        return Err(ElfError::BadProgramHeaders);
    }
    let table_end = phoff
        .checked_add(phnum * PHDR)
        .ok_or(ElfError::BadProgramHeaders)?;
    if table_end > bytes.len() {
        return Err(ElfError::BadProgramHeaders);
    }

    let mut segments = Vec::new();
    for i in 0..phnum {
        let at = phoff + i * PHDR;
        let kind = u32_at(bytes, at)?;
        match kind {
            PT_LOAD => {}
            PT_DYNAMIC | PT_INTERP => return Err(ElfError::Dynamic),
            PT_TLS => return Err(ElfError::ThreadLocal),
            _ => continue,
        }
        let flags = u32_at(bytes, at + 4)?;
        let offset = u64_at(bytes, at + 8)?;
        let vaddr = u64_at(bytes, at + 16)?;
        let filesz = u64_at(bytes, at + 32)?;
        let memsz = u64_at(bytes, at + 40)?;
        if memsz == 0 {
            continue;
        }
        if filesz > memsz {
            return Err(ElfError::BadSegment);
        }
        let file_end = offset.checked_add(filesz).ok_or(ElfError::BadSegment)?;
        if file_end > bytes.len() as u64 {
            return Err(ElfError::BadSegment);
        }
        let end = vaddr
            .checked_add(memsz)
            .and_then(|e| e.checked_add(PAGE - 1))
            .ok_or(ElfError::OutOfRange)?;
        if vaddr < allowed.start || end & !(PAGE - 1) > allowed.end {
            return Err(ElfError::OutOfRange);
        }
        let segment = Segment {
            vaddr,
            memsz,
            offset,
            filesz,
            read: flags & PF_R != 0,
            write: flags & PF_W != 0,
            execute: flags & PF_X != 0,
        };
        if segment.write && segment.execute {
            return Err(ElfError::WriteExecute);
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        return Err(ElfError::NoSegments);
    }
    segments.sort_by_key(|s| s.vaddr);
    for pair in segments.windows(2) {
        if pair[0].page_end() > pair[1].page_start() {
            return Err(ElfError::Overlap);
        }
    }
    if !segments
        .iter()
        .any(|s| s.execute && entry >= s.vaddr && entry < s.vaddr + s.memsz)
    {
        return Err(ElfError::BadEntry);
    }
    Ok(Image { entry, segments })
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::vec;

    use super::*;

    pub(crate) struct Ph {
        pub kind: u32,
        pub flags: u32,
        pub offset: u64,
        pub vaddr: u64,
        pub filesz: u64,
        pub memsz: u64,
    }

    pub(crate) fn load(flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> Ph {
        Ph {
            kind: PT_LOAD,
            flags,
            offset,
            vaddr,
            filesz,
            memsz,
        }
    }

    /// An ELF64 image with the given entry and program headers, padded to `len` bytes.
    pub(crate) fn image(entry: u64, phdrs: &[Ph], len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len.max(EHDR + phdrs.len() * PHDR)];
        b[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        b[24..32].copy_from_slice(&entry.to_le_bytes());
        b[32..40].copy_from_slice(&(EHDR as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes());
        b[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
        for (i, p) in phdrs.iter().enumerate() {
            let at = EHDR + i * PHDR;
            b[at..at + 4].copy_from_slice(&p.kind.to_le_bytes());
            b[at + 4..at + 8].copy_from_slice(&p.flags.to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&p.offset.to_le_bytes());
            b[at + 16..at + 24].copy_from_slice(&p.vaddr.to_le_bytes());
            b[at + 32..at + 40].copy_from_slice(&p.filesz.to_le_bytes());
            b[at + 40..at + 48].copy_from_slice(&p.memsz.to_le_bytes());
        }
        b
    }

    const RANGE: core::ops::Range<u64> = 0x1000..0x1000_0000_0000;

    #[test]
    fn kk_t4_elf_parses_a_static_executable() {
        let img = image(
            0x20_1000,
            &[
                load(PF_R, 0, 0x20_0000, 0x100, 0x100),
                load(PF_R | PF_X, 0x1000, 0x20_1000, 0x200, 0x200),
                load(PF_R | PF_W, 0x2000, 0x20_3000, 0x10, 0x5000),
                Ph {
                    kind: 0x6474_e551,
                    flags: 6,
                    offset: 0,
                    vaddr: 0,
                    filesz: 0,
                    memsz: 0,
                },
                load(PF_R, 0, 0x30_0000, 0, 0),
            ],
            0x3000,
        );
        let parsed = parse(&img, RANGE).expect("parses");
        assert_eq!(parsed.entry, 0x20_1000);
        assert_eq!(parsed.segments.len(), 3);
        let data = parsed.segments[2];
        assert!(data.write && !data.execute && data.read);
        assert_eq!(data.page_start(), 0x20_3000);
        assert_eq!(data.page_end(), 0x20_8000);
    }

    #[test]
    fn kk_t4_elf_refusals() {
        let text = |vaddr| load(PF_R | PF_X, 0, vaddr, 0x10, 0x10);
        let good = image(0x20_0000, &[text(0x20_0000)], 0x100);
        assert!(parse(&good, RANGE).is_ok());

        assert_eq!(parse(&good[..8], RANGE), Err(ElfError::Truncated));
        assert_eq!(parse(&good[..32], RANGE), Err(ElfError::Truncated));
        let mut b = good.clone();
        b[0] = 0;
        assert_eq!(parse(&b, RANGE), Err(ElfError::NotElf));
        let mut b = good.clone();
        b[4] = 1;
        assert_eq!(parse(&b, RANGE), Err(ElfError::Not64Bit));
        let mut b = good.clone();
        b[5] = 2;
        assert_eq!(parse(&b, RANGE), Err(ElfError::NotLittleEndian));
        let mut b = good.clone();
        b[16] = 3;
        assert_eq!(parse(&b, RANGE), Err(ElfError::NotExecutable));
        let mut b = good.clone();
        b[18] = 3;
        assert_eq!(parse(&b, RANGE), Err(ElfError::WrongMachine));
        let mut b = good.clone();
        b[54] = 32;
        assert_eq!(parse(&b, RANGE), Err(ElfError::BadProgramHeaders));
        let mut b = good.clone();
        b[56] = 2;
        b.truncate(EHDR + PHDR + 10);
        assert_eq!(parse(&b, RANGE), Err(ElfError::BadProgramHeaders));
        let mut b = good.clone();
        b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(parse(&b, RANGE), Err(ElfError::BadProgramHeaders)));

        let interp = Ph {
            kind: PT_INTERP,
            flags: PF_R,
            offset: 0,
            vaddr: 0,
            filesz: 0,
            memsz: 0,
        };
        assert_eq!(
            parse(&image(0x20_0000, &[text(0x20_0000), interp], 0x100), RANGE),
            Err(ElfError::Dynamic)
        );
        let tls = Ph {
            kind: PT_TLS,
            flags: PF_R,
            offset: 0,
            vaddr: 0,
            filesz: 0,
            memsz: 8,
        };
        assert_eq!(
            parse(&image(0x20_0000, &[text(0x20_0000), tls], 0x100), RANGE),
            Err(ElfError::ThreadLocal)
        );
        assert_eq!(
            parse(&image(0x20_0000, &[], 0x100), RANGE),
            Err(ElfError::NoSegments)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_X, 0, 0x20_0000, 0x20, 0x10)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::BadSegment)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_X, 0x80, 0x20_0000, 0x100, 0x100)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::BadSegment)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_X, u64::MAX, 0x20_0000, 2, 2)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::BadSegment)
        );
        assert_eq!(
            parse(&image(0, &[text(0)], 0x100), RANGE),
            Err(ElfError::OutOfRange)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_X, 0, 0x20_0000, 0, u64::MAX)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::OutOfRange)
        );
        assert_eq!(
            parse(
                &image(0x1000_0000_0000, &[text(0x1000_0000_0000)], 0x100),
                RANGE
            ),
            Err(ElfError::OutOfRange)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[text(0x20_0000), load(PF_R, 0, 0x20_0800, 0x10, 0x10)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::Overlap)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_W | PF_X, 0, 0x20_0000, 0x10, 0x10)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::WriteExecute)
        );
        assert_eq!(
            parse(
                &image(
                    0x20_0000,
                    &[load(PF_R | PF_W, 0, 0x20_0000, 0x10, 0x10)],
                    0x100
                ),
                RANGE
            ),
            Err(ElfError::BadEntry)
        );
        assert_eq!(
            parse(&image(0x20_0010, &[text(0x20_0000)], 0x100), RANGE),
            Err(ElfError::BadEntry)
        );
        for e in [
            ElfError::Truncated,
            ElfError::NotElf,
            ElfError::Not64Bit,
            ElfError::NotLittleEndian,
            ElfError::NotExecutable,
            ElfError::WrongMachine,
            ElfError::BadProgramHeaders,
            ElfError::Dynamic,
            ElfError::ThreadLocal,
            ElfError::NoSegments,
            ElfError::BadSegment,
            ElfError::OutOfRange,
            ElfError::Overlap,
            ElfError::WriteExecute,
            ElfError::BadEntry,
        ] {
            assert!(!e.describe().is_empty());
        }
    }
}
