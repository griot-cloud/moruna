//! AD-T3 device_no_copy (AD-I2): a kernel that returns a Torch CUDA tensor is not copied and the
//! payload's tier is `Device`.
//!
//! Tagged "(reference host, E1)" in 05-adapters section k. No GPU host exists (preamble section
//! 7, E1), so this test is present and ignored rather than passed; it closes on the reference
//! host when one is available.

#![cfg(feature = "python")]

#[test]
#[ignore = "reference host, E1: no GPU host exists, so a CUDA tensor cannot be produced"]
fn ad_t3_device_no_copy() {
    unimplemented!("reference host, E1");
}
