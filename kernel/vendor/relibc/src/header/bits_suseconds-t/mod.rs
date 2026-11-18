#[cfg(any(target_os = "linux", target_os = "moruna"))]
use crate::platform::types::c_long;

#[cfg(not(any(target_os = "linux", target_os = "moruna")))]
use crate::platform::types::c_int;

#[cfg(any(target_os = "linux", target_os = "moruna"))]
/// Used for time in microseconds.
pub type suseconds_t = c_long;
#[cfg(not(any(target_os = "linux", target_os = "moruna")))]
/// Used for time in microseconds.
pub type suseconds_t = c_int;
