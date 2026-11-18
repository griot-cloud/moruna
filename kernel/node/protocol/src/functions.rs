//! The functions a job uses, as the engine is given them (system design 3E, 5C; PM, item 3.6).
//!
//! `node-admission` fetches and verifies each registered function the job names
//! (`kernel/node/functions`) and starts the engine with, beside its other grants:
//!
//! - one read-only memory object per function, the registered file itself, under the slot
//!   [`slot`]`(N)`: `function.0`, `function.1`, ..., in the order of the job's
//!   `needs.functions`, at most [`MAX`];
//! - the index, a memory object under [`INDEX_SLOT`] holding one line per function,
//!   `N NAME SHA256 SIZE\n`: its slot's number, its registered name, its SHA-256 in lowercase
//!   hex, and the file's length in bytes (a memory object is whole pages). The text ends at
//!   the object's first zero byte, within its first [`INDEX_MAX`] bytes.
//!
//! The slots carry numbers, not names, because a start slot's name is at most 32 bytes and a
//! function's 64. The engine's supervisor hands both on as they are (`grant functions
//! functions`, `kernel/engine`), and the engine reads the index to find a function by name. A
//! job that uses no function is given no index.

use alloc::{format, string::String, vec::Vec};

/// The slot of the index.
pub const INDEX_SLOT: &str = "functions";
/// The most functions a job is given.
pub const MAX: usize = 16;
/// The most bytes of the index read: a page holds sixteen lines with room to spare.
pub const INDEX_MAX: usize = 4096;
/// The longest registered name (`moruna-api`'s `FunctionRegistration`).
pub const NAME_MAX: usize = 64;

/// The slot of function `n`: `function.N`.
pub fn slot(n: usize) -> String {
    format!("function.{n}")
}

/// One line of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its registered name.
    pub name: String,
    /// The file's SHA-256, which `node-admission` checked against the registration.
    pub sha256: [u8; 32],
    /// The file's length in bytes.
    pub size: u64,
}

/// Why an index is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// More than [`MAX`] functions, or an index longer than [`INDEX_MAX`].
    TooMany,
    /// A line that is not `N NAME SHA256 SIZE`, with the line's number (from 1) and why.
    Line(usize, &'static str),
    /// A name given twice.
    Twice(String),
}

/// Whether `name` is a registered function's: 1 to 64 characters, a lowercase letter first,
/// then lowercase letters, digits, `_`, `-` and `.`.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=NAME_MAX).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(b))
}

fn hex(sha256: &[u8; 32]) -> String {
    sha256.iter().map(|b| format!("{b:02x}")).collect()
}

/// The index of `entries`, in their order: what `node-admission` writes.
pub fn index(entries: &[Entry]) -> Result<String, IndexError> {
    if entries.len() > MAX {
        return Err(IndexError::TooMany);
    }
    let mut text = String::new();
    for (n, entry) in entries.iter().enumerate() {
        if !valid_name(&entry.name) {
            return Err(IndexError::Line(n + 1, "not a registered function's name"));
        }
        if entries[..n].iter().any(|e| e.name == entry.name) {
            return Err(IndexError::Twice(entry.name.clone()));
        }
        if entry.size == 0 {
            return Err(IndexError::Line(n + 1, "a function's file is not empty"));
        }
        text.push_str(&format!(
            "{n} {} {} {}\n",
            entry.name,
            hex(&entry.sha256),
            entry.size
        ));
    }
    if text.len() > INDEX_MAX {
        return Err(IndexError::TooMany);
    }
    Ok(text)
}

fn digest(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let value = |d: u8| match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in digits.chunks(2).enumerate() {
        out[i] = (value(pair[0])? << 4) | value(pair[1])?;
    }
    Some(out)
}

/// The entries of the index `text`, in slot order: what the engine reads. Every line is
/// checked: its number is its place, its name a registered function's and given once, its
/// SHA-256 64 lowercase hex digits, its size a positive decimal; anything else is refused.
pub fn parse(text: &str) -> Result<Vec<Entry>, IndexError> {
    if text.len() > INDEX_MAX {
        return Err(IndexError::TooMany);
    }
    let mut entries: Vec<Entry> = Vec::new();
    for (i, line) in text.split_terminator('\n').enumerate() {
        let n = i + 1;
        if entries.len() == MAX {
            return Err(IndexError::TooMany);
        }
        let words: Vec<&str> = line.split(' ').collect();
        let [number, name, sha256, size] = words.as_slice() else {
            return Err(IndexError::Line(n, "a line is `N NAME SHA256 SIZE`"));
        };
        if *number != format!("{i}") {
            return Err(IndexError::Line(n, "a line's number is its place, from 0"));
        }
        if !valid_name(name) {
            return Err(IndexError::Line(n, "not a registered function's name"));
        }
        if entries.iter().any(|e| e.name == *name) {
            return Err(IndexError::Twice(String::from(*name)));
        }
        let sha256 =
            digest(sha256).ok_or(IndexError::Line(n, "a SHA-256 is 64 lowercase hex digits"))?;
        let size = size
            .parse::<u64>()
            .ok()
            .filter(|s| *s > 0 && !size.starts_with('0') && !size.starts_with('+'))
            .ok_or(IndexError::Line(n, "a size is a positive decimal"))?;
        entries.push(Entry {
            name: String::from(*name),
            sha256,
            size,
        });
    }
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(IndexError::Line(
            entries.len(),
            "the index ends with a newline",
        ));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn entry(name: &str, byte: u8, size: u64) -> Entry {
        Entry {
            name: name.into(),
            sha256: [byte; 32],
            size,
        }
    }

    #[test]
    fn an_index_is_written_and_read_back_in_slot_order() {
        let entries = vec![entry("gate-score", 0xab, 4_194_304), entry("add_one", 7, 1)];
        let text = index(&entries).unwrap();
        assert_eq!(
            text,
            format!(
                "0 gate-score {} 4194304\n1 add_one {} 1\n",
                "ab".repeat(32),
                "07".repeat(32)
            )
        );
        assert_eq!(parse(&text), Ok(entries));
        assert_eq!(parse(""), Ok(Vec::new()));
        assert_eq!(index(&[]), Ok(String::new()));
        assert_eq!(
            (slot(0), slot(15)),
            ("function.0".into(), "function.15".into())
        );
        assert!(slot(MAX - 1).len() <= 32, "a start slot's name");
    }

    #[test]
    fn sixteen_fit_a_page_and_a_seventeenth_is_refused() {
        let long = |i: usize| format!("{}{i:02}", "f".repeat(NAME_MAX - 2));
        let most: Vec<Entry> = (0..MAX)
            .map(|i| entry(&long(i), i as u8, u64::MAX))
            .collect();
        let text = index(&most).unwrap();
        assert!(text.len() <= INDEX_MAX);
        assert_eq!(parse(&text).unwrap().len(), MAX);
        let mut more = most.clone();
        more.push(entry("one-more", 1, 1));
        assert_eq!(index(&more), Err(IndexError::TooMany));
        let extra = format!("{text}{MAX} one-more {} 1\n", "00".repeat(32));
        assert_eq!(parse(&extra), Err(IndexError::TooMany));
        assert_eq!(parse(&"x".repeat(INDEX_MAX + 1)), Err(IndexError::TooMany));
    }

    #[test]
    fn a_line_that_is_not_one_is_refused_with_its_number() {
        let sha = "ab".repeat(32);
        let good = format!("0 f {sha} 10\n");
        assert!(parse(&good).is_ok());
        for (text, line, why) in [
            (format!("0 f {sha}\n"), 1, "`N NAME SHA256 SIZE`"),
            (format!("0 f {sha} 10 extra\n"), 1, "`N NAME SHA256 SIZE`"),
            (format!("1 f {sha} 10\n"), 1, "its place"),
            (format!("{good}0 g {sha} 10\n"), 2, "its place"),
            (format!("0 F {sha} 10\n"), 1, "name"),
            (format!("0 1f {sha} 10\n"), 1, "name"),
            (format!("0 {} {sha} 10\n", "f".repeat(65)), 1, "name"),
            (format!("0 f {} 10\n", "AB".repeat(32)), 1, "SHA-256"),
            (format!("0 f {} 10\n", "ab".repeat(31)), 1, "SHA-256"),
            (format!("0 f {sha} 0\n"), 1, "size"),
            (format!("0 f {sha} 010\n"), 1, "size"),
            (format!("0 f {sha} +10\n"), 1, "size"),
            (format!("0 f {sha} ten\n"), 1, "size"),
            (format!("0 f {sha} 10"), 1, "newline"),
            (format!("0  f {sha} 10\n"), 1, "`N NAME SHA256 SIZE`"),
        ] {
            match parse(&text) {
                Err(IndexError::Line(at, said)) => {
                    assert_eq!(at, line, "{text:?}");
                    assert!(said.contains(why), "{text:?}: {said}");
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
        assert_eq!(
            parse(&format!("{good}1 f {sha} 10\n")),
            Err(IndexError::Twice("f".into()))
        );
    }

    #[test]
    fn admission_writes_only_what_the_engine_reads() {
        assert_eq!(
            index(&[entry("F", 0, 1)]),
            Err(IndexError::Line(1, "not a registered function's name"))
        );
        assert_eq!(
            index(&[entry("f", 0, 1), entry("f", 1, 1)]),
            Err(IndexError::Twice("f".into()))
        );
        assert_eq!(
            index(&[entry("f", 0, 0)]),
            Err(IndexError::Line(1, "a function's file is not empty"))
        );
        assert!(valid_name("a") && valid_name("gate.score-2_x") && !valid_name(""));
    }
}
