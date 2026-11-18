//! moruna: the kernel's errors (`moruna_abi::Error`, its positive codes) as `io::Error`s.

use moruna_sys::Error;

use crate::io;

/// The kernel's calls return their error; nothing keeps a thread's last one.
pub fn errno() -> io::RawOsError {
    0
}

pub fn is_interrupted(code: io::RawOsError) -> bool {
    code == Error::Interrupted.code() as io::RawOsError
}

fn error(code: io::RawOsError) -> Option<Error> {
    u32::try_from(code).ok().and_then(Error::from_code)
}

pub fn decode_error_kind(code: io::RawOsError) -> io::ErrorKind {
    use io::ErrorKind::*;
    match error(code) {
        Some(Error::BadHandle | Error::WrongType | Error::InvalidArgs | Error::OutOfRange) => {
            InvalidInput
        }
        Some(Error::AccessDenied) => PermissionDenied,
        Some(Error::NoMemory) => OutOfMemory,
        Some(Error::ShouldWait) => WouldBlock,
        Some(Error::PeerClosed) => BrokenPipe,
        Some(Error::TimedOut) => TimedOut,
        Some(Error::BadSyscall) => Unsupported,
        Some(Error::Limit | Error::QuotaExceeded) => QuotaExceeded,
        Some(Error::Interrupted) => Interrupted,
        Some(Error::BufferTooSmall | Error::BadState | Error::Fault) | None => Uncategorized,
    }
}

pub fn error_string(code: io::RawOsError) -> String {
    match error(code) {
        Some(error) => error.name().to_string(),
        None => format!("unknown error {code}"),
    }
}
