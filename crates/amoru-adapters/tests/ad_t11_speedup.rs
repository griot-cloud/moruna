//! AD-T11 speedup (S8): a NumPy kernel that releases the GIL reaches at least 5.6 times the
//! single worker throughput on 8 workers.
//!
//! Tagged "(reference host, E1; free-threaded only)" in 05-adapters section k. A throughput ratio
//! is a timing figure, and preamble section 7 (E1) says a timing gate is measured on the
//! reference host; this test is present and ignored rather than measured here.

#![cfg(feature = "python")]

#[test]
#[ignore = "reference host, E1: a throughput ratio is measured on the reference host only"]
fn ad_t11_speedup() {
    unimplemented!("reference host, E1");
}
