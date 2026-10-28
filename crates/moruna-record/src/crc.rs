//! crc32c (Castagnoli), the checksum a record carries (design 21 a.2).

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

/// The 256-entry table, generated when the crate is compiled.
const fn table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 == 1 { (c >> 1) ^ POLY } else { c >> 1 };
            bit += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = table();

/// The crc32c of `data`: initial value all ones, reflected, final xor all ones.
/// `crc32c(b"123456789") == 0xE3069283`.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = TABLE[((c as u8) ^ b) as usize] ^ (c >> 8);
    }
    !c
}
