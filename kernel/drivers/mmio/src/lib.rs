//! Width-exact access to a mapped window (SDD 25, f.5): device registers, PCI
//! configuration space, and the rings a driver shares with its device.
//!
//! A device register is read and written at its own width, once, never byte by byte,
//! and never through a Rust reference, because the device changes it underneath. On
//! the kernel a [`Region`] is a `moruna_svc::Mapping`, whose accessors are single
//! volatile loads and stores; on a host it is [`Fake`], so the PCI and virtio code
//! above it is tested as ordinary code.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use moruna_abi::Error;

/// A mapped window, read and written at natural alignment and exact width.
pub trait Region {
    /// The window's length in bytes.
    fn len(&self) -> usize;
    fn read8(&self, offset: usize) -> Result<u8, Error>;
    fn read16(&self, offset: usize) -> Result<u16, Error>;
    fn read32(&self, offset: usize) -> Result<u32, Error>;
    fn read64(&self, offset: usize) -> Result<u64, Error>;
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error>;
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error>;
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error>;
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error>;

    /// Whether the window is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A 64-bit register a device exposes as two 32-bit halves, low half first.
    fn write64_split(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.write32(offset, value as u32)?;
        self.write32(offset + 4, (value >> 32) as u32)
    }

    /// Copies `bytes` in at `offset`, a byte at a time.
    fn write_bytes(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        for (i, b) in bytes.iter().enumerate() {
            self.write8(offset + i, *b)?;
        }
        Ok(())
    }

    /// Copies `out.len()` bytes out from `offset`, a byte at a time.
    fn read_bytes(&self, offset: usize, out: &mut [u8]) -> Result<(), Error> {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.read8(offset + i)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "none")]
impl Region for moruna_svc::Mapping<'_> {
    fn len(&self) -> usize {
        moruna_svc::Mapping::len(self) as usize
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        moruna_svc::Mapping::read8(self, offset)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        moruna_svc::Mapping::read16(self, offset)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        moruna_svc::Mapping::read32(self, offset)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        moruna_svc::Mapping::read64(self, offset)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        moruna_svc::Mapping::write8(self, offset, value)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        moruna_svc::Mapping::write16(self, offset, value)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        moruna_svc::Mapping::write32(self, offset, value)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        moruna_svc::Mapping::write64(self, offset, value)
    }
}

/// A part of another window: `len` bytes from `base`. Out of its bounds is
/// `OutOfRange`, whatever the window below holds there.
pub struct Sub<'a, R: Region + ?Sized> {
    region: &'a R,
    base: usize,
    len: usize,
}

impl<'a, R: Region + ?Sized> Sub<'a, R> {
    /// `len` bytes of `region` from `base`; `OutOfRange` past its end.
    pub fn new(region: &'a R, base: usize, len: usize) -> Result<Sub<'a, R>, Error> {
        match base.checked_add(len) {
            Some(end) if end <= region.len() => Ok(Sub { region, base, len }),
            _ => Err(Error::OutOfRange),
        }
    }

    fn at(&self, offset: usize, n: usize) -> Result<usize, Error> {
        match offset.checked_add(n) {
            Some(end) if end <= self.len => Ok(self.base + offset),
            _ => Err(Error::OutOfRange),
        }
    }
}

impl<R: Region + ?Sized> Region for Sub<'_, R> {
    fn len(&self) -> usize {
        self.len
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.region.read8(self.at(offset, 1)?)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.region.read16(self.at(offset, 2)?)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.region.read32(self.at(offset, 4)?)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.region.read64(self.at(offset, 8)?)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.region.write8(self.at(offset, 1)?, value)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        self.region.write16(self.at(offset, 2)?, value)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        self.region.write32(self.at(offset, 4)?, value)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.region.write64(self.at(offset, 8)?, value)
    }
}

#[cfg(not(target_os = "none"))]
pub use fake::Fake;

/// A window of plain memory with the same alignment and bounds rules as a mapping:
/// what host tests put a device's registers and rings in.
#[cfg(not(target_os = "none"))]
mod fake {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::{Error, Region};

    pub struct Fake {
        bytes: RefCell<Vec<u8>>,
        writable: bool,
    }

    impl Fake {
        /// `len` zero bytes, writable.
        pub fn new(len: usize) -> Fake {
            Fake {
                bytes: RefCell::new(alloc::vec![0; len]),
                writable: true,
            }
        }

        /// `len` zero bytes that refuse writes, as a read-only mapping does.
        pub fn read_only(len: usize) -> Fake {
            Fake {
                bytes: RefCell::new(alloc::vec![0; len]),
                writable: false,
            }
        }

        /// Sets bytes behind the window's back, as a device does.
        pub fn poke(&self, offset: usize, bytes: &[u8]) {
            self.bytes.borrow_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
        }

        /// Reads bytes behind the window's back.
        pub fn peek(&self, offset: usize, n: usize) -> Vec<u8> {
            self.bytes.borrow()[offset..offset + n].to_vec()
        }

        fn at(&self, offset: usize, n: usize) -> Result<usize, Error> {
            if !offset.is_multiple_of(n) {
                return Err(Error::InvalidArgs);
            }
            match offset.checked_add(n) {
                Some(end) if end <= self.bytes.borrow().len() => Ok(offset),
                _ => Err(Error::OutOfRange),
            }
        }

        fn load<const N: usize>(&self, offset: usize) -> Result<[u8; N], Error> {
            let at = self.at(offset, N)?;
            let mut out = [0u8; N];
            out.copy_from_slice(&self.bytes.borrow()[at..at + N]);
            Ok(out)
        }

        fn store(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
            let at = self.at(offset, bytes.len())?;
            if !self.writable {
                return Err(Error::AccessDenied);
            }
            self.bytes.borrow_mut()[at..at + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }

    impl Region for Fake {
        fn len(&self) -> usize {
            self.bytes.borrow().len()
        }
        fn read8(&self, offset: usize) -> Result<u8, Error> {
            self.load::<1>(offset).map(|b| b[0])
        }
        fn read16(&self, offset: usize) -> Result<u16, Error> {
            self.load(offset).map(u16::from_le_bytes)
        }
        fn read32(&self, offset: usize) -> Result<u32, Error> {
            self.load(offset).map(u32::from_le_bytes)
        }
        fn read64(&self, offset: usize) -> Result<u64, Error> {
            self.load(offset).map(u64::from_le_bytes)
        }
        fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
            self.store(offset, &[value])
        }
        fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
            self.store(offset, &value.to_le_bytes())
        }
        fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
            self.store(offset, &value.to_le_bytes())
        }
        fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
            self.store(offset, &value.to_le_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dr_t3_fake_keeps_width_alignment_and_bounds() {
        let f = Fake::new(32);
        assert!(!f.is_empty());
        f.write8(0, 0x11).expect("8");
        f.write16(2, 0x2233).expect("16");
        f.write32(4, 0x4455_6677).expect("32");
        f.write64(8, 0x8899_aabb_ccdd_eeff).expect("64");
        assert_eq!(f.read8(0), Ok(0x11));
        assert_eq!(f.read16(2), Ok(0x2233));
        assert_eq!(f.read32(4), Ok(0x4455_6677));
        assert_eq!(f.read64(8), Ok(0x8899_aabb_ccdd_eeff));
        assert_eq!(f.peek(2, 2), [0x33, 0x22]);
        assert_eq!(f.read16(1), Err(Error::InvalidArgs));
        assert_eq!(f.read32(2), Err(Error::InvalidArgs));
        assert_eq!(f.read64(4), Err(Error::InvalidArgs));
        assert_eq!(f.read64(32), Err(Error::OutOfRange));
        assert_eq!(f.write32(30, 0), Err(Error::InvalidArgs));
        assert_eq!(f.write32(32, 0), Err(Error::OutOfRange));
        assert_eq!(f.read8(usize::MAX), Err(Error::OutOfRange));
        f.write64_split(16, 0x0102_0304_0506_0708).expect("split");
        assert_eq!(f.read32(16), Ok(0x0506_0708));
        assert_eq!(f.read32(20), Ok(0x0102_0304));
        f.write_bytes(24, b"abc").expect("bytes");
        let mut out = [0u8; 3];
        f.read_bytes(24, &mut out).expect("bytes");
        assert_eq!(&out, b"abc");
        f.poke(28, &[9, 8]);
        assert_eq!(f.read16(28), Ok(0x0809));
        let ro = Fake::read_only(8);
        assert_eq!(ro.write8(0, 1), Err(Error::AccessDenied));
        assert_eq!(ro.write16(0, 1), Err(Error::AccessDenied));
        assert_eq!(ro.write64(0, 1), Err(Error::AccessDenied));
        assert_eq!(ro.read8(0), Ok(0));
    }

    #[test]
    fn dr_t3_a_sub_window_is_bounded_by_itself() {
        let f = Fake::new(64);
        assert!(Sub::new(&f, 60, 8).is_err());
        assert!(Sub::new(&f, usize::MAX, 2).is_err());
        let s = Sub::new(&f, 16, 16).expect("sub");
        assert_eq!(s.len(), 16);
        s.write8(0, 1).expect("8");
        s.write16(2, 2).expect("16");
        s.write32(4, 3).expect("32");
        s.write64(8, 4).expect("64");
        assert_eq!(f.read8(16), Ok(1));
        assert_eq!(f.read16(18), Ok(2));
        assert_eq!(f.read32(20), Ok(3));
        assert_eq!(f.read64(24), Ok(4));
        assert_eq!(s.read8(0), Ok(1));
        assert_eq!(s.read16(2), Ok(2));
        assert_eq!(s.read32(4), Ok(3));
        assert_eq!(s.read64(8), Ok(4));
        assert_eq!(s.read64(16), Err(Error::OutOfRange));
        assert_eq!(s.write8(16, 0), Err(Error::OutOfRange));
        assert_eq!(s.read8(usize::MAX), Err(Error::OutOfRange));
    }
}
