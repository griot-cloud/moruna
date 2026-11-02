//! Paths (SDD 28, f.5): a relative path is taken from the working directory or from an
//! open directory, as the `*at` calls say; the image filesystem resolves what is left
//! (`..` and links included, SDD 27 f.6), so nothing here is followed or normalised.

use alloc::vec::Vec;

/// The longest path, with its NUL, as Linux's `PATH_MAX`.
pub const PATH_MAX: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// `ENOENT`: an empty path.
    Empty,
    /// `ENAMETOOLONG`.
    TooLong,
}

/// `path` taken from `base` (an absolute directory) when it is relative.
pub fn join(base: &[u8], path: &[u8]) -> Result<Vec<u8>, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    let mut out = Vec::new();
    if path[0] != b'/' {
        out.extend_from_slice(base);
        if out.last() != Some(&b'/') {
            out.push(b'/');
        }
    }
    out.extend_from_slice(path);
    if out.len() >= PATH_MAX {
        return Err(PathError::TooLong);
    }
    Ok(out)
}

/// The last part of a path, for `/proc/self/exe` and messages.
pub fn base_name(path: &[u8]) -> &[u8] {
    let trimmed = match path.iter().rposition(|b| *b != b'/') {
        Some(end) => &path[..=end],
        None => return path,
    };
    match trimmed.iter().rposition(|b| *b == b'/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t7_join() {
        assert_eq!(join(b"/", b"etc/passwd").unwrap(), b"/etc/passwd");
        assert_eq!(join(b"/usr", b"lib").unwrap(), b"/usr/lib");
        assert_eq!(join(b"/usr/", b"lib").unwrap(), b"/usr/lib");
        assert_eq!(join(b"/usr", b"/abs").unwrap(), b"/abs");
        assert_eq!(join(b"/usr", b"../x").unwrap(), b"/usr/../x");
        assert_eq!(join(b"/", b""), Err(PathError::Empty));
        let long = alloc::vec![b'a'; PATH_MAX];
        assert_eq!(join(b"/", &long), Err(PathError::TooLong));
        assert_eq!(base_name(b"/usr/local/bin/python3"), b"python3");
        assert_eq!(base_name(b"/usr/lib/"), b"lib");
        assert_eq!(base_name(b"name"), b"name");
        assert_eq!(base_name(b"/"), b"/");
    }
}
