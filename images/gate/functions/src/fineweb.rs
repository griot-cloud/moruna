//! FineWeb MinHash (`bench/vm/ooc_workloads.py`, `fw_minhash` and `_signature`): word 5-gram
//! shingles hashed with CRC-32, a 64-permutation MinHash signature and 8 LSH band keys.
//!
//! The permutations are NumPy's `default_rng(20261114)` draws, exported bit for bit
//! (`data/minhash_ab.bin`). NumPy's `uint64` arithmetic wraps, so `(h * a + b) % p` is taken on
//! the wrapped values, as here. Every value is an integer: the port matches bit for bit.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cols::Strs;
use crate::error::Result;

pub const PERMS: usize = 64;
pub const BANDS: usize = 8;
const ROWS: usize = PERMS / BANDS;
/// `_PRIME`, the Mersenne prime 2**61 - 1.
pub const PRIME: u64 = (1 << 61) - 1;
const BAND_MUL: u64 = 0x9E37_79B1;

const AB: &[u8] = include_bytes!("../data/minhash_ab.bin");
const _: () = assert!(
    AB.len() == 2 * PERMS * 8,
    "minhash_ab.bin is not two tables of 64"
);

const fn u64_at(i: usize) -> u64 {
    let b = i * 8;
    u64::from_le_bytes([
        AB[b],
        AB[b + 1],
        AB[b + 2],
        AB[b + 3],
        AB[b + 4],
        AB[b + 5],
        AB[b + 6],
        AB[b + 7],
    ])
}

const fn table(from: usize) -> [u64; PERMS] {
    let mut out = [0; PERMS];
    let mut i = 0;
    while i < PERMS {
        out[i] = u64_at(from + i);
        i += 1;
    }
    out
}

/// `_A`, `_B`: the permutations.
pub static A: [u64; PERMS] = table(0);
pub static B: [u64; PERMS] = table(PERMS);

/// zlib's CRC-32 (IEEE, reflected, polynomial 0xEDB88320).
static CRC: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// A CRC-32 being fed: `zlib.crc32` of everything fed.
#[derive(Clone, Copy)]
pub struct Crc(u32);

impl Crc {
    pub fn new() -> Crc {
        Crc(0xFFFF_FFFF)
    }

    #[inline]
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

impl Default for Crc {
    fn default() -> Crc {
        Crc::new()
    }
}

/// `zlib.crc32(bytes)`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.feed(bytes);
    c.finish()
}

/// Python's `str.isspace()`: Unicode's White_Space, and the four information separators
/// U+001C to U+001F, which Python counts and Rust does not.
#[inline]
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `s.split()`: the runs of non-whitespace.
pub fn py_split(s: &str) -> impl Iterator<Item = &str> {
    s.split(py_space).filter(|w| !w.is_empty())
}

/// `x % (2**61 - 1)` for any `u64`, as `np.uint64`'s `%` gives it: 2**61 is 1 modulo the prime.
#[inline]
fn mod_prime(x: u64) -> u64 {
    let r = (x & PRIME) + (x >> 61);
    if r >= PRIME { r - PRIME } else { r }
}

/// `_signature(text)`: the 64 permutation minima of the text's word 5-gram shingles, low 32 bits.
pub fn signature(text: &str) -> [u32; PERMS] {
    let lower = text.to_lowercase();
    let mut words: Vec<&str> = py_split(&lower).collect();
    while words.len() < 5 {
        words.push("");
    }
    let mut mins = [u64::MAX; PERMS];
    // A set of shingles in Python; a shingle seen twice gives the same minima, so each window is
    // taken as it comes.
    for w in words.windows(5) {
        let mut c = Crc::new();
        for (k, word) in w.iter().enumerate() {
            if k > 0 {
                c.feed(b" ");
            }
            c.feed(word.as_bytes());
        }
        let h = u64::from(c.finish());
        for j in 0..PERMS {
            let v = mod_prime(h.wrapping_mul(A[j]).wrapping_add(B[j]));
            if v < mins[j] {
                mins[j] = v;
            }
        }
    }
    let mut out = [0u32; PERMS];
    for (o, m) in out.iter_mut().zip(mins) {
        *o = (m & 0xFFFF_FFFF) as u32;
    }
    out
}

/// The band keys of one signature: each band's eight values times 0x9E3779B1, XORed, then
/// XORed with the band's number.
pub fn bands(sig: &[u32; PERMS]) -> [u64; BANDS] {
    let mut out = [0u64; BANDS];
    for (band, key) in out.iter_mut().enumerate() {
        let mut k = 0u64;
        for &v in &sig[band * ROWS..(band + 1) * ROWS] {
            k ^= u64::from(v).wrapping_mul(BAND_MUL);
        }
        *key = k ^ band as u64;
    }
    out
}

/// `fw_minhash`: each text's signature and band keys, flat (`n * 64` and `n * 8`). A null text
/// is `""`.
pub fn minhash(texts: &Strs) -> Result<(Vec<u32>, Vec<u64>)> {
    let n = texts.len();
    let mut sigs = Vec::with_capacity(n * PERMS);
    let mut keys = Vec::with_capacity(n * BANDS);
    for i in 0..n {
        let sig = signature(texts.get(i)?.unwrap_or(""));
        keys.extend_from_slice(&bands(&sig));
        sigs.extend_from_slice(&sig);
    }
    Ok((sigs, keys))
}

/// For tests: the shingles Python builds (`" ".join(words[i : i + 5])`), joined.
pub fn shingles(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut words: Vec<&str> = py_split(&lower).collect();
    while words.len() < 5 {
        words.push("");
    }
    words.windows(5).map(|w| w.join(" ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_zlibs() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"hello world"), 0x0d4a_1185);
        assert_eq!(crc32("äb".as_bytes()), {
            let mut c = Crc::default();
            c.feed("ä".as_bytes());
            c.feed(b"b");
            c.finish()
        });
    }

    #[test]
    fn the_permutations_read_at_run_time_as_at_compile_time() {
        assert_eq!(table(0), A);
        assert_eq!(table(PERMS), B);
        assert_eq!(u64_at(0), A[0]);
        assert!(A.iter().all(|a| *a >= 1 && *a < PRIME));
    }

    #[test]
    fn signature_and_bands() {
        let s = signature("The quick brown fox jumps over the lazy dog");
        assert_eq!(
            s,
            signature("the  QUICK brown\tfox jumps over the lazy dog")
        );
        let b = bands(&s);
        assert_eq!(b.len(), BANDS);
        let texts = ["a b c d e f", "x"];
        let offsets = [0i32, 11, 12];
        let data = b"a b c d e fx";
        let strs = Strs {
            offsets: crate::cols::Offsets::Small(&offsets),
            data,
            valid: None,
        };
        let (sigs, keys) = minhash(&strs).unwrap();
        assert_eq!(&sigs[..PERMS], &signature(texts[0])[..]);
        assert_eq!(keys.len(), 2 * BANDS);
    }

    #[test]
    fn mersenne_modulo() {
        for x in [
            0u64,
            1,
            PRIME - 1,
            PRIME,
            PRIME + 1,
            u64::MAX,
            u64::MAX - 7,
            1 << 61,
            3 << 61,
        ] {
            assert_eq!(mod_prime(x), x % PRIME, "{x}");
        }
    }

    #[test]
    fn python_whitespace() {
        assert_eq!(
            py_split(" a\u{1c}b\u{3000}c\t").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(shingles("A b"), ["a b   "]);
        assert_eq!(shingles(""), ["    "]);
        assert_eq!(shingles("1 2 3 4 5 6").len(), 2);
    }
}
