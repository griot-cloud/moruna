//! What the CPU offers ring's AES-GCM, and which of ring's implementations that makes it take
//! (work item 6.3ab).
//!
//! The network server is built for `x86_64-moruna-user`, whose features are SSE and SSE2 only, and
//! ring 0.17.14 chooses its AES-GCM at run time from CPUID (`OPENSSL_cpuid_setup`,
//! `cpu::intel::cpuid_to_caps_and_set_c_flags`, `aead::aes_gcm::DynKey::new`). So whether the
//! node's TLS has AES-NI is the CPU's answer, read here with the same rule:
//!
//! - AES-NI, PCLMULQDQ and SSSE3, with AVX2, VAES and VPCLMULQDQ: `vaes-avx2`;
//! - the same without those three, with AVX and MOVBE: `aesni-avx-movbe`;
//! - AES-NI, PCLMULQDQ and SSSE3 alone: `aesni-clmul`;
//! - SSSE3 without them: `vpaes`, with GHASH in software;
//! - none: `software`, ring's constant-time AES and GHASH in C, tens of cycles a byte.
//!
//! ring counts AVX, AVX2, VAES and VPCLMULQDQ only when the OS has turned on the YMM state
//! (XCR0's bits 1 and 2). A program cannot read XCR0 without `xgetbv`; the size CPUID's leaf 0xD
//! gives for the state XCR0 enables says the same: 832 bytes or more once the YMM state is on.

use alloc::{format, string::String};

/// The XSAVE area's size, legacy region, header and the YMM state's upper halves: what leaf
/// 0xD's EBX reaches once XCR0 enables the YMM state.
const XSAVE_WITH_YMM: u32 = 512 + 64 + 256;

/// The CPUID words ring reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cpuid {
    /// Leaf 1's ECX.
    pub leaf1_ecx: u32,
    /// Leaf 7 (subleaf 0)'s EBX and ECX; zero when the CPU has no leaf 7.
    pub leaf7_ebx: u32,
    pub leaf7_ecx: u32,
    /// Leaf 0xD (subleaf 0)'s EBX: the bytes of the state XCR0 enables; zero without the leaf.
    pub xsave_bytes: u32,
}

fn bit(word: u32, n: u32) -> bool {
    word & (1 << n) != 0
}

impl Cpuid {
    pub fn aes(&self) -> bool {
        bit(self.leaf1_ecx, 25)
    }
    pub fn pclmulqdq(&self) -> bool {
        bit(self.leaf1_ecx, 1)
    }
    pub fn ssse3(&self) -> bool {
        bit(self.leaf1_ecx, 9)
    }
    pub fn movbe(&self) -> bool {
        bit(self.leaf1_ecx, 22)
    }
    /// The OS has turned the YMM state on: OSXSAVE, and XCR0's state reaches the YMM area.
    pub fn ymm(&self) -> bool {
        bit(self.leaf1_ecx, 27) && self.xsave_bytes >= XSAVE_WITH_YMM
    }
    pub fn avx(&self) -> bool {
        bit(self.leaf1_ecx, 28) && self.ymm()
    }
    pub fn avx2(&self) -> bool {
        bit(self.leaf7_ebx, 5) && self.ymm()
    }
    /// VAES and VPCLMULQDQ, which ring takes together and only with AVX.
    pub fn vaes_vpclmulqdq(&self) -> bool {
        bit(self.leaf7_ecx, 9) && bit(self.leaf7_ecx, 10) && self.avx()
    }

    /// The AES-GCM ring 0.17.14 takes on this CPU.
    pub fn aes_gcm(&self) -> &'static str {
        if self.aes() && self.pclmulqdq() && self.ssse3() {
            if self.avx2() && self.vaes_vpclmulqdq() {
                "vaes-avx2"
            } else if self.avx() && self.movbe() {
                "aesni-avx-movbe"
            } else {
                "aesni-clmul"
            }
        } else if self.ssse3() {
            "vpaes"
        } else {
            "software"
        }
    }

    /// The console's words: the features, and ring's choice.
    pub fn line(&self) -> String {
        let f = |b: bool| u8::from(b);
        format!(
            "KE2 netstack: crypto: aes={} pclmulqdq={} ssse3={} ymm={} avx={} avx2={} movbe={} \
             vaes+vpclmulqdq={}: ring's AES-GCM is {}",
            f(self.aes()),
            f(self.pclmulqdq()),
            f(self.ssse3()),
            f(self.ymm()),
            f(self.avx()),
            f(self.avx2()),
            f(self.movbe()),
            f(self.vaes_vpclmulqdq()),
            self.aes_gcm(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AES: u32 = 1 << 25;
    const PCLMULQDQ: u32 = 1 << 1;
    const SSSE3: u32 = 1 << 9;
    const MOVBE: u32 = 1 << 22;
    const OSXSAVE: u32 = 1 << 27;
    const AVX: u32 = 1 << 28;
    const AVX2: u32 = 1 << 5;
    const VAES: u32 = 1 << 9;
    const VPCLMULQDQ: u32 = 1 << 10;

    fn cpu(leaf1_ecx: u32, leaf7_ebx: u32, leaf7_ecx: u32, xsave_bytes: u32) -> Cpuid {
        Cpuid {
            leaf1_ecx,
            leaf7_ebx,
            leaf7_ecx,
            xsave_bytes,
        }
    }

    #[test]
    fn ring_takes_each_aes_gcm_by_its_rule() {
        let all = AES | PCLMULQDQ | SSSE3 | MOVBE | OSXSAVE | AVX;
        // An Ice Lake server, as Azure's v5 sizes have: VAES with AVX2.
        let ice_lake = cpu(all, AVX2, VAES | VPCLMULQDQ, 2696);
        assert_eq!(ice_lake.aes_gcm(), "vaes-avx2");
        // A Skylake or Cascade Lake: no VAES.
        assert_eq!(cpu(all, AVX2, 0, 1088).aes_gcm(), "aesni-avx-movbe");
        // The same CPU with the YMM state off: AVX does not count.
        assert_eq!(
            cpu(all, AVX2, VAES | VPCLMULQDQ, 576).aes_gcm(),
            "aesni-clmul"
        );
        assert_eq!(
            cpu(all & !OSXSAVE, AVX2, VAES | VPCLMULQDQ, 2696).aes_gcm(),
            "aesni-clmul"
        );
        // AVX without MOVBE.
        assert_eq!(cpu(all & !MOVBE, 0, 0, 832).aes_gcm(), "aesni-clmul");
        // No AES-NI, or no PCLMULQDQ: vector-permute AES.
        assert_eq!(cpu(all & !AES, AVX2, 0, 832).aes_gcm(), "vpaes");
        assert_eq!(cpu(all & !PCLMULQDQ, AVX2, 0, 832).aes_gcm(), "vpaes");
        // Nothing past SSE2.
        assert_eq!(Cpuid::default().aes_gcm(), "software");
    }

    #[test]
    fn the_line_says_the_features_and_the_choice() {
        let c = cpu(
            AES | PCLMULQDQ | SSSE3 | MOVBE | OSXSAVE | AVX,
            AVX2,
            VAES | VPCLMULQDQ,
            832,
        );
        assert_eq!(
            c.line(),
            "KE2 netstack: crypto: aes=1 pclmulqdq=1 ssse3=1 ymm=1 avx=1 avx2=1 movbe=1 \
             vaes+vpclmulqdq=1: ring's AES-GCM is vaes-avx2"
        );
        assert_eq!(
            Cpuid::default().line(),
            "KE2 netstack: crypto: aes=0 pclmulqdq=0 ssse3=0 ymm=0 avx=0 avx2=0 movbe=0 \
             vaes+vpclmulqdq=0: ring's AES-GCM is software"
        );
    }
}
