//! The initramfs: an uncompressed cpio archive in the "newc" format (magic `070701`,
//! or `070702` with checksums, which are not verified), as `cpio -o -H newc` writes it.

use core::str;

const HEADER: usize = 110;
const MAGIC_NEWC: &[u8; 6] = b"070701";
const MAGIC_CRC: &[u8; 6] = b"070702";
const TRAILER: &str = "TRAILER!!!";
const MODE_TYPE: u32 = 0o170000;
const MODE_FILE: u32 = 0o100000;
const MODE_DIR: u32 = 0o040000;

/// Why an archive was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpioError {
    /// The archive ends inside a header, name or file.
    Truncated,
    /// A header does not start with a newc magic.
    BadMagic,
    /// A header field is not eight hexadecimal digits.
    BadField,
    /// A name is empty, not NUL terminated or not UTF-8.
    BadName,
    /// The archive has no trailer.
    NoTrailer,
    /// An entry is neither a regular file nor a directory.
    UnsupportedType,
}

impl CpioError {
    pub fn describe(self) -> &'static str {
        match self {
            CpioError::Truncated => "the archive ends inside an entry",
            CpioError::BadMagic => "an entry header has no newc magic",
            CpioError::BadField => "an entry header field is not hexadecimal",
            CpioError::BadName => "an entry name is empty, unterminated or not UTF-8",
            CpioError::NoTrailer => "the archive has no trailer",
            CpioError::UnsupportedType => "an entry is neither a file nor a directory",
        }
    }
}

/// One regular file of the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File<'a> {
    /// The path, without a leading `./` or `/`.
    pub path: &'a str,
    pub data: &'a [u8],
}

fn field(header: &[u8], index: usize) -> Result<u32, CpioError> {
    let start = 6 + index * 8;
    let digits = header.get(start..start + 8).ok_or(CpioError::Truncated)?;
    let text = str::from_utf8(digits).map_err(|_| CpioError::BadField)?;
    u32::from_str_radix(text, 16).map_err(|_| CpioError::BadField)
}

fn align4(n: usize) -> Option<usize> {
    n.checked_add(3).map(|v| v & !3)
}

/// The files of an archive, in archive order. Directories are skipped; anything else
/// that is not a regular file is refused, so a symlink cannot alias a path.
pub fn files(archive: &[u8]) -> Result<alloc::vec::Vec<File<'_>>, CpioError> {
    let mut out = alloc::vec::Vec::new();
    let mut at = 0usize;
    loop {
        let header = archive.get(at..at + HEADER).ok_or(if at >= archive.len() {
            CpioError::NoTrailer
        } else {
            CpioError::Truncated
        })?;
        let magic = &header[..6];
        if magic != MAGIC_NEWC && magic != MAGIC_CRC {
            return Err(CpioError::BadMagic);
        }
        let mode = field(header, 1)?;
        let filesize = field(header, 6)? as usize;
        let namesize = field(header, 11)? as usize;
        if namesize < 2 {
            return Err(CpioError::BadName);
        }
        let name_start = at + HEADER;
        let name_end = name_start
            .checked_add(namesize)
            .ok_or(CpioError::Truncated)?;
        let name_bytes = archive
            .get(name_start..name_end)
            .ok_or(CpioError::Truncated)?;
        let (name, nul) = name_bytes.split_at(namesize - 1);
        if nul != [0] {
            return Err(CpioError::BadName);
        }
        let name = str::from_utf8(name).map_err(|_| CpioError::BadName)?;
        let data_start = align4(name_end).ok_or(CpioError::Truncated)?;
        let data_end = data_start
            .checked_add(filesize)
            .ok_or(CpioError::Truncated)?;
        let data = archive
            .get(data_start..data_end)
            .ok_or(CpioError::Truncated)?;
        if name == TRAILER {
            return Ok(out);
        }
        match mode & MODE_TYPE {
            MODE_FILE => {
                let path = name.trim_start_matches("./").trim_start_matches('/');
                if path.is_empty() {
                    return Err(CpioError::BadName);
                }
                out.push(File { path, data });
            }
            MODE_DIR => {}
            _ => return Err(CpioError::UnsupportedType),
        }
        at = align4(data_end).ok_or(CpioError::Truncated)?;
    }
}

/// The contents of the file at `path`, if the archive holds one.
pub fn find<'a>(files: &[File<'a>], path: &str) -> Option<&'a [u8]> {
    files.iter().find(|f| f.path == path).map(|f| f.data)
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::{format, vec::Vec};

    use super::*;

    /// Writes one newc entry, the way `cpio -o -H newc` does.
    pub(crate) fn entry(out: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
        let header = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            1,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out.extend_from_slice(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    pub(crate) fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, mode, data) in entries {
            entry(&mut out, name, *mode, data);
        }
        entry(&mut out, TRAILER, 0, b"");
        out
    }

    #[test]
    fn kk_t3_cpio_reads_files_and_skips_directories() {
        let a = archive(&[
            (".", 0o040755, b""),
            ("./manifest", 0o100644, b"moruna-manifest 1\n"),
            ("bin", 0o040755, b""),
            ("bin/ping", 0o100755, b"\x7fELF-ish"),
        ]);
        let f = files(&a).expect("reads");
        assert_eq!(f.len(), 2);
        assert_eq!(find(&f, "manifest"), Some(&b"moruna-manifest 1\n"[..]));
        assert_eq!(find(&f, "bin/ping"), Some(&b"\x7fELF-ish"[..]));
        assert_eq!(find(&f, "bin/pong"), None);
    }

    #[test]
    fn kk_t3_cpio_refusals() {
        assert_eq!(files(b""), Err(CpioError::NoTrailer));
        let good = archive(&[("a", 0o100644, b"abc")]);
        assert_eq!(files(&good[..50]), Err(CpioError::Truncated));
        assert_eq!(files(&good[..HEADER + 1]), Err(CpioError::Truncated));
        assert_eq!(files(&good[..HEADER + 3]), Err(CpioError::Truncated));
        let mut no_trailer = Vec::new();
        entry(&mut no_trailer, "a", 0o100644, b"abc");
        assert_eq!(files(&no_trailer), Err(CpioError::NoTrailer));
        let mut bad = good.clone();
        bad[0] = b'1';
        assert_eq!(files(&bad), Err(CpioError::BadMagic));
        let mut bad = good.clone();
        bad[14] = b'z';
        assert_eq!(files(&bad), Err(CpioError::BadField));
        let link = archive(&[("l", 0o120777, b"target")]);
        assert_eq!(files(&link), Err(CpioError::UnsupportedType));
        let mut unterminated = Vec::new();
        entry(&mut unterminated, "ab", 0o100644, b"");
        unterminated[HEADER + 2] = b'c';
        assert_eq!(files(&unterminated), Err(CpioError::BadName));
        let mut empty = Vec::new();
        entry(&mut empty, "", 0o100644, b"");
        assert_eq!(files(&empty), Err(CpioError::BadName));
        let slash = archive(&[("./", 0o100644, b"")]);
        assert_eq!(files(&slash), Err(CpioError::BadName));
        let mut not_utf8 = Vec::new();
        entry(&mut not_utf8, "ab", 0o100644, b"");
        not_utf8[HEADER] = 0xff;
        assert_eq!(files(&not_utf8), Err(CpioError::BadName));
        let mut crc = good.clone();
        crc[5] = b'2';
        assert_eq!(files(&crc).map(|f| f.len()), Ok(1));
        for e in [
            CpioError::Truncated,
            CpioError::BadMagic,
            CpioError::BadField,
            CpioError::BadName,
            CpioError::NoTrailer,
            CpioError::UnsupportedType,
        ] {
            assert!(!e.describe().is_empty());
        }
    }
}
