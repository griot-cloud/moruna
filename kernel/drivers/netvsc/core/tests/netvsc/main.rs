//! Item 1.11a's host tests: the card against a fake of the host's side (`fake`), over an
//! in-memory channel.
//!
//! - `initialise`: NVSP and RNDIS initialisation, the version step-down and the refusal
//!   below NVSP 5, the address and the MTU.
//! - `traffic`: a thousand frames each way, the send buffer and the ring running out and
//!   recovering, a virtual function offered and ignored, the link going down and up.
//! - `refusal`: what no well-behaved host sends, refused without a panic.
//! - `fuzz`: ten thousand random host packets, from a fixed seed.

mod fake;
mod fuzz;
mod initialise;
mod refusal;
mod traffic;
