//! Real native SIMD/FPU isolation participant, paired through boot.image.
#![no_std]
#![no_main]
#![deny(unsafe_code)]
#[path = "../fpu_fixture.rs"]
mod fixture;
moruna_sys::main!(run);
fn run() -> i64 {
    fixture::run(610795520)
}
