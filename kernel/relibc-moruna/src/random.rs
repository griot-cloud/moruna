//! `getrandom`, from the CPU's random source (`RANDOM_READ`). It never blocks, so every flag
//! Linux takes is satisfied as given.

use crate::errno::EINVAL;
use crate::os;
use crate::{Errno, Result, from_kernel};

pub const GRND_NONBLOCK: u32 = 1;
pub const GRND_RANDOM: u32 = 2;
pub const GRND_INSECURE: u32 = 4;

/// Fills `buf`: its length.
pub fn fill(buf: &mut [u8], flags: u32) -> Result<usize> {
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
        return Err(Errno(EINVAL));
    }
    os::random(buf).map_err(from_kernel)?;
    Ok(buf.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_is_filled_and_unknown_flags_are_refused() {
        let mut buf = [0u8; 300];
        assert_eq!(fill(&mut buf, GRND_NONBLOCK | GRND_RANDOM), Ok(300));
        assert!(buf.iter().any(|b| *b != 0));
        assert_eq!(fill(&mut [], 0), Ok(0));
        assert_eq!(fill(&mut buf, 8), Err(Errno(EINVAL)));
    }
}
