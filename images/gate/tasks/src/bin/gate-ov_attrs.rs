//! The gate's `ov_attrs` as a native function (images/gate/README.md).

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    moruna_gate_tasks::serve(moruna_gate_functions::batch::ov_attrs)
}
