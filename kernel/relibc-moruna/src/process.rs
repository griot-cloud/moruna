//! The process: its end, its name, and what `uname` says of the system. A task is one process
//! with no parent, group or session of its own, run as no user in particular.

use crate::os;

/// Ends the process with `code`, the task's exit code.
pub fn exit(code: i32) -> ! {
    os::exit(code)
}

/// The process's name, from its start page: its first argument.
pub fn name() -> &'static str {
    os::name()
}

/// The process's environment, std's (its start slot `env`, item 5.5): each entry `NAME=VALUE` as
/// a C string, valid for the process's life, which `environ` is built from.
pub fn environment() -> impl Iterator<Item = *const u8> {
    os::environment()
}

/// The process id every task has, as the first process of a system has on Linux.
pub const PID: i32 = 1;

/// What `uname` says: the system, the node (the process's name), the release (the kernel's
/// ABI version), the version and the machine.
pub fn uname() -> [Field; 5] {
    let abi = os::abi_version();
    let mut release = Field::new();
    release.push_u64(abi >> 16);
    release.push(b".");
    release.push_u64(abi & 0xffff);
    [
        Field::from(b"Moruna"),
        Field::from(name().as_bytes()),
        release,
        Field::from(b"moruna-sys"),
        Field::from(b"x86_64"),
    ]
}

/// One of `uname`'s fields: at most 64 bytes, as `struct utsname` holds them with their NUL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    bytes: [u8; Field::MAX],
    len: usize,
}

impl Field {
    pub const MAX: usize = 64;

    fn new() -> Field {
        Field {
            bytes: [0; Field::MAX],
            len: 0,
        }
    }

    fn from(text: &[u8]) -> Field {
        let mut f = Field::new();
        f.push(text);
        f
    }

    /// Appends `text`, cut so a NUL still fits after it.
    fn push(&mut self, text: &[u8]) {
        let n = text.len().min(Field::MAX - 1 - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&text[..n]);
        self.len += n;
    }

    fn push_u64(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push(&digits[i..]);
    }

    /// The bytes, without the NUL.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_names_the_system_the_task_and_the_abi() {
        let u = uname();
        let text: std::vec::Vec<&[u8]> = u.iter().map(Field::as_bytes).collect();
        assert_eq!(
            text,
            [&b"Moruna"[..], b"host", b"2.8", b"moruna-sys", b"x86_64"]
        );
        assert_eq!(name(), "host");
    }

    #[test]
    fn the_environment_is_each_entry_as_a_c_string() {
        let entries: std::vec::Vec<&[u8]> = environment()
            // SAFETY: each is a NUL-terminated entry the process keeps.
            .map(|p| unsafe { core::ffi::CStr::from_ptr(p.cast()) }.to_bytes())
            .collect();
        assert_eq!(entries, [&b"MORUNA_ROLE=function"[..]]);
    }

    #[test]
    fn exit_ends_with_the_code() {
        let ended = std::panic::catch_unwind(|| exit(7)).unwrap_err();
        assert_eq!(ended.downcast_ref::<i32>(), Some(&7));
        assert_eq!(PID, 1);
    }

    #[test]
    fn a_field_keeps_room_for_its_nul() {
        let long = [b'a'; 100];
        let f = Field::from(&long);
        assert_eq!(f.as_bytes().len(), Field::MAX - 1);
        let mut n = Field::new();
        n.push_u64(0);
        n.push_u64(u64::MAX);
        assert_eq!(n.as_bytes(), b"018446744073709551615");
    }
}
