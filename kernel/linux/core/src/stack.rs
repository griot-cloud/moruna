//! A Linux program's first stack (SDD 28, f.2), as Linux's `execve` lays it out: from
//! the stack pointer up, `argc`, the argument pointers and a null, the environment
//! pointers and a null, the auxiliary vector ending in `AT_NULL`, and above them the
//! strings, the platform name and sixteen random bytes. The stack pointer is 16-byte
//! aligned.

use alloc::vec::Vec;

/// Auxiliary vector types.
pub mod at {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const PLATFORM: u64 = 15;
    pub const HWCAP: u64 = 16;
    pub const CLKTCK: u64 = 17;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const HWCAP2: u64 = 26;
    pub const EXECFN: u64 = 31;
}

/// What the program is started with.
pub struct Start<'a> {
    pub argv: &'a [&'a [u8]],
    pub envp: &'a [&'a [u8]],
    pub execfn: &'a [u8],
    pub random: [u8; 16],
    /// The vector's entries, without the three the builder points at its own bytes
    /// (`AT_EXECFN`, `AT_PLATFORM`, `AT_RANDOM`) and without `AT_NULL`.
    pub auxv: &'a [(u64, u64)],
}

/// The most bytes of arguments and environment, as Linux's `ARG_MAX` would allow.
pub const ARGS_MAX: usize = 128 * 1024;

/// The stack's bytes and where they go: they end at `top` (16-byte aligned) and begin
/// at the returned stack pointer. `None` when the strings pass `ARGS_MAX` or a string
/// holds a NUL.
pub fn build(top: u64, start: &Start<'_>) -> Option<(u64, Vec<u8>)> {
    let strings: usize = start
        .argv
        .iter()
        .chain(start.envp.iter())
        .map(|s| s.len() + 1)
        .sum::<usize>()
        + start.execfn.len()
        + 1;
    if strings > ARGS_MAX
        || start
            .argv
            .iter()
            .chain(start.envp.iter())
            .chain(core::iter::once(&start.execfn))
            .any(|s| s.contains(&0))
    {
        return None;
    }
    // From the top down: an 8-byte zero, the strings, the platform, the random bytes.
    let mut cursor = top.checked_sub(8)?;
    let mut place = |bytes: &[u8], out: &mut Vec<(u64, Vec<u8>)>| -> Option<u64> {
        cursor = cursor.checked_sub(bytes.len() as u64)?;
        out.push((cursor, bytes.to_vec()));
        Some(cursor)
    };
    let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new();
    let with_nul = |s: &[u8]| {
        let mut v = s.to_vec();
        v.push(0);
        v
    };
    let execfn = place(&with_nul(start.execfn), &mut pieces)?;
    let mut envp = Vec::new();
    for e in start.envp.iter().rev() {
        envp.push(place(&with_nul(e), &mut pieces)?);
    }
    envp.reverse();
    let mut argv = Vec::new();
    for a in start.argv.iter().rev() {
        argv.push(place(&with_nul(a), &mut pieces)?);
    }
    argv.reverse();
    let platform = place(b"x86_64\0", &mut pieces)?;
    let random = place(&start.random, &mut pieces)?;

    let mut auxv: Vec<(u64, u64)> = start.auxv.to_vec();
    auxv.push((at::EXECFN, execfn));
    auxv.push((at::PLATFORM, platform));
    auxv.push((at::RANDOM, random));
    auxv.push((at::NULL, 0));
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * auxv.len();
    let sp = (cursor.checked_sub(words as u64 * 8)?) & !0xf;

    let mut bytes = alloc::vec![0u8; (top - sp) as usize];
    let mut word = sp;
    let mut put = |v: u64, bytes: &mut Vec<u8>| {
        let at = (word - sp) as usize;
        bytes[at..at + 8].copy_from_slice(&v.to_le_bytes());
        word += 8;
    };
    put(argv.len() as u64, &mut bytes);
    for p in &argv {
        put(*p, &mut bytes);
    }
    put(0, &mut bytes);
    for p in &envp {
        put(*p, &mut bytes);
    }
    put(0, &mut bytes);
    for (k, v) in &auxv {
        put(*k, &mut bytes);
        put(*v, &mut bytes);
    }
    for (addr, piece) in pieces {
        let at = (addr - sp) as usize;
        bytes[at..at + piece.len()].copy_from_slice(&piece);
    }
    Some((sp, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(bytes: &[u8], sp: u64, addr: u64) -> u64 {
        let at = (addr - sp) as usize;
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    fn string(bytes: &[u8], sp: u64, addr: u64) -> Vec<u8> {
        let at = (addr - sp) as usize;
        bytes[at..]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect()
    }

    #[test]
    fn lx_t4_stack_as_linux_lays_it_out() {
        let top = 0x7ffe_0000_0000u64;
        let start = Start {
            argv: &[b"python3", b"-c", b"print(1)"],
            envp: &[b"PATH=/usr/bin", b"LANG=C.UTF-8"],
            execfn: b"/usr/local/bin/python3",
            random: [7; 16],
            auxv: &[(at::PAGESZ, 4096), (at::ENTRY, 0x401000)],
        };
        let (sp, bytes) = build(top, &start).unwrap();
        assert_eq!(sp % 16, 0);
        assert_eq!(sp + bytes.len() as u64, top);
        assert_eq!(word(&bytes, sp, sp), 3);
        assert_eq!(string(&bytes, sp, word(&bytes, sp, sp + 8)), b"python3");
        assert_eq!(string(&bytes, sp, word(&bytes, sp, sp + 24)), b"print(1)");
        assert_eq!(word(&bytes, sp, sp + 32), 0);
        assert_eq!(
            string(&bytes, sp, word(&bytes, sp, sp + 40)),
            b"PATH=/usr/bin"
        );
        assert_eq!(word(&bytes, sp, sp + 56), 0);
        // The vector: what was given, then the builder's three, then AT_NULL.
        let mut at_ = sp + 64;
        let mut seen = Vec::new();
        loop {
            let (k, v) = (word(&bytes, sp, at_), word(&bytes, sp, at_ + 8));
            seen.push((k, v));
            at_ += 16;
            if k == at::NULL {
                break;
            }
        }
        assert_eq!(seen.len(), 6);
        assert_eq!(seen[0], (at::PAGESZ, 4096));
        let find = |k| {
            seen.iter()
                .find(|(kk, _)| *kk == k)
                .map(|(_, v)| *v)
                .unwrap()
        };
        assert_eq!(
            string(&bytes, sp, find(at::EXECFN)),
            b"/usr/local/bin/python3"
        );
        assert_eq!(string(&bytes, sp, find(at::PLATFORM)), b"x86_64");
        let r = find(at::RANDOM);
        assert_eq!(&bytes[(r - sp) as usize..(r - sp) as usize + 16], &[7; 16]);
        assert_eq!(word(&bytes, sp, top - 8), 0);
    }

    #[test]
    fn lx_t4_stack_refusals() {
        let big = alloc::vec![b'a'; ARGS_MAX];
        let argv: [&[u8]; 1] = [&big];
        let s = Start {
            argv: &argv,
            envp: &[],
            execfn: b"/x",
            random: [0; 16],
            auxv: &[],
        };
        assert!(build(0x7ffe_0000_0000, &s).is_none());
        let s = Start {
            argv: &[b"a\0b"],
            envp: &[],
            execfn: b"/x",
            random: [0; 16],
            auxv: &[],
        };
        assert!(build(0x7ffe_0000_0000, &s).is_none());
        let s = Start {
            argv: &[b"a"],
            envp: &[],
            execfn: b"/x",
            random: [0; 16],
            auxv: &[],
        };
        assert!(build(16, &s).is_none());
    }
}
