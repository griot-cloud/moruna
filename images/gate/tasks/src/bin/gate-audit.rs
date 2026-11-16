//! The gate's `audit` as a native task (images/gate/README.md).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

moruna_sys::main!(run);

fn run() -> i64 {
    moruna_gate_tasks::run(moruna_gate_record::Job::Audit)
}
