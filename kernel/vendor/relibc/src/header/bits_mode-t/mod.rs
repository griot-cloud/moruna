#[cfg(any(target_os = "linux", target_os = "moruna"))]
use crate::platform::types::c_uint;

#[cfg(not(any(target_os = "linux", target_os = "moruna")))]
use crate::platform::types::c_int;

/// Used for some file attributes.
#[cfg(any(target_os = "linux", target_os = "moruna"))]
pub type mode_t = c_uint;
/// Used for some file attributes.
#[cfg(not(any(target_os = "linux", target_os = "moruna")))]
pub type mode_t = c_int;
