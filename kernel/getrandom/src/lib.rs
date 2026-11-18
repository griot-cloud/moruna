//! The `getrandom` crate's custom backend on Moruna (item 5.0): random bytes from the core's
//! `random_read`, the one source of entropy, under the core's control.
//!
//! `getrandom` knows no `target_os = "moruna"`. `kernel/tools/build-std.sh` builds every program
//! with `--cfg getrandom_backend="custom"`, and this crate defines the function that backend
//! calls, `__getrandom_v03_custom`. A program that reaches `getrandom` 0.3 links it:
//!
//! ```ignore
//! use moruna_getrandom as _;
//! ```
//!
//! The bytes come from std's own `moruna_sys` (`std::os::moruna`), so nothing here makes a call
//! of its own. A crate std depends on is unstable to its users (`rustc_private`), as Hermit's
//! `hermit_abi` is through `std::os::hermit`, so this crate opts in on Moruna.

#![no_std]
#![cfg_attr(target_os = "moruna", feature(rustc_private))]

#[cfg(target_os = "moruna")]
extern crate std;

/// Fills `dest` from `source`, as getrandom's custom backend contract asks: a failure is
/// getrandom's custom error carrying the core's error code.
pub fn fill(
    dest: &mut [u8],
    source: impl FnOnce(&mut [u8]) -> Result<(), u32>,
) -> Result<(), getrandom::Error> {
    source(dest)
        .map_err(|code| getrandom::Error::new_custom(u16::try_from(code).unwrap_or(u16::MAX)))
}

/// getrandom 0.3's custom backend: `len` random bytes at `dest`, from the core's `random_read`.
///
/// # Safety
///
/// getrandom's contract: `dest` is valid for writes of `len` bytes, which need not be
/// initialized.
#[cfg(target_os = "moruna")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: the caller's contract; zeroed first, the bytes are initialized for the slice.
    let dest = unsafe {
        dest.write_bytes(0, len);
        core::slice::from_raw_parts_mut(dest, len)
    };
    fill(dest, |buf| {
        std::os::moruna::moruna_sys::random(buf).map_err(|e| e.code())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_s_bytes_are_given() {
        let mut dest = [0u8; 4];
        fill(&mut dest, |buf| {
            buf.copy_from_slice(&[1, 2, 3, 4]);
            Ok(())
        })
        .unwrap();
        assert_eq!(dest, [1, 2, 3, 4]);
    }

    #[test]
    fn the_core_s_error_is_getrandom_s_custom_error_with_its_code() {
        // `random_read` on a CPU with no random source answers `BadState`, code 13.
        let error = fill(&mut [0u8; 8], |_| Err(13)).unwrap_err();
        assert_eq!(error, getrandom::Error::new_custom(13));
        // A code past u16 is the last custom code, never another one.
        let error = fill(&mut [0u8; 1], |_| Err(70_000)).unwrap_err();
        assert_eq!(error, getrandom::Error::new_custom(u16::MAX));
    }
}
