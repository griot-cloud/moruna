//! Descriptors 0 to 2, the task's standard streams, and no others: standard input is empty,
//! standard output and error are the task's log (5B: std owns it, and C writes through std's
//! writer). Each is a character device, not a terminal. A function opens no files (5A0).

use crate::errno::{EBADF, ENOTTY, ESPIPE};
use crate::os;
use crate::{Errno, Result};

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
pub const STDERR: i32 = 2;

pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;

/// A character device's file type, `S_IFCHR`.
pub const S_IFCHR: u32 = 0o020_000;

/// Whether `fd` is one of the three.
pub fn is_open(fd: i32) -> bool {
    (STDIN..=STDERR).contains(&fd)
}

fn open(fd: i32) -> Result<()> {
    if is_open(fd) {
        Ok(())
    } else {
        Err(Errno(EBADF))
    }
}

/// `write`: all of `bytes`, to the log for 1 and 2.
pub fn write(fd: i32, bytes: &[u8]) -> Result<usize> {
    match fd {
        STDOUT | STDERR => Ok(os::write_out(bytes)),
        _ => Err(Errno(EBADF)),
    }
}

/// `read`: the end at once from 0.
pub fn read(fd: i32, _buf: &mut [u8]) -> Result<usize> {
    match fd {
        STDIN => Ok(0),
        _ => Err(Errno(EBADF)),
    }
}

/// `fstat`'s mode: a character device, readable for 0 and writable for 1 and 2.
pub fn mode(fd: i32) -> Result<u32> {
    open(fd)?;
    Ok(S_IFCHR | if fd == STDIN { 0o444 } else { 0o222 })
}

/// `fcntl(F_GETFL)`.
pub fn status_flags(fd: i32) -> Result<i32> {
    open(fd)?;
    Ok(if fd == STDIN { O_RDONLY } else { O_WRONLY })
}

/// `lseek`: a stream has no position.
pub fn seek(fd: i32) -> Result<i64> {
    open(fd)?;
    Err(Errno(ESPIPE))
}

/// A terminal's `ioctl`: none of the three is one.
pub fn terminal(fd: i32) -> Result<()> {
    open(fd)?;
    Err(Errno(ENOTTY))
}

/// `close`: the stream stays, as the log does, and the call succeeds.
pub fn close(fd: i32) -> Result<()> {
    open(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_goes_to_the_log_and_input_is_empty() {
        assert_eq!(write(STDOUT, b"one "), Ok(4));
        assert_eq!(write(STDERR, b"two\n"), Ok(4));
        assert_eq!(os::written(), b"one two\n");
        assert_eq!(write(STDIN, b"x"), Err(Errno(EBADF)));
        assert_eq!(write(3, b"x"), Err(Errno(EBADF)));
        let mut buf = [0u8; 4];
        assert_eq!(read(STDIN, &mut buf), Ok(0));
        assert_eq!(read(STDOUT, &mut buf), Err(Errno(EBADF)));
    }

    #[test]
    fn the_three_are_character_devices_and_no_others_are_open() {
        assert_eq!(mode(STDIN), Ok(S_IFCHR | 0o444));
        assert_eq!(mode(STDERR), Ok(S_IFCHR | 0o222));
        assert_eq!(status_flags(STDIN), Ok(O_RDONLY));
        assert_eq!(status_flags(STDOUT), Ok(O_WRONLY));
        assert_eq!(seek(STDOUT), Err(Errno(ESPIPE)));
        assert_eq!(terminal(STDERR), Err(Errno(ENOTTY)));
        assert_eq!(close(STDIN), Ok(()));
        for fd in [-1, 3, 100] {
            assert!(!is_open(fd));
            assert_eq!(mode(fd), Err(Errno(EBADF)));
            assert_eq!(status_flags(fd), Err(Errno(EBADF)));
            assert_eq!(seek(fd), Err(Errno(EBADF)));
            assert_eq!(terminal(fd), Err(Errno(EBADF)));
            assert_eq!(close(fd), Err(Errno(EBADF)));
        }
    }
}
