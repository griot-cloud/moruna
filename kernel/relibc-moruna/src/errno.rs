//! Linux's errno values, the ones this crate answers with. relibc's `Sys` checks at compile time
//! that its own headers give the same.

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EINTR: i32 = 4;
pub const EIO: i32 = 5;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const EINVAL: i32 = 22;
pub const ENOTTY: i32 = 25;
pub const ESPIPE: i32 = 29;
pub const EPIPE: i32 = 32;
pub const ERANGE: i32 = 34;
pub const ENOSYS: i32 = 38;
pub const ENODEV: i32 = 19;
pub const ETIMEDOUT: i32 = 110;
