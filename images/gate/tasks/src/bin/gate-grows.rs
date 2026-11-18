//! A test function, not one of the gate's jobs (6.3g's restart proof, system design 3A): `audit`,
//! whose memory depends on its input's values, as a function's does when the data outgrows its
//! measurement in production. A batch with a trip picked up on or after 16 January 2024 holds
//! [`GROWN_BYTES`] more for its call, written page by page; every other batch holds what `audit`
//! holds. A month read from its start warms the function up on its first days, so its measured
//! room is `audit`'s, and the second half of the month needs more than that room: the task's
//! charge is refused, and the stage must give it a larger room and finish. Its answers are
//! `audit`'s, whatever it held.
//!
//! The image's stack is built beside the gate's (the booted turn's scripts); the gate's own image
//! (`image.sh`) does not carry it.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use moruna_function::RecordBatch;

/// What a batch with a trip of the second half of the month holds beside `audit`'s own.
const GROWN_BYTES: usize = 40 << 20;

/// 16 January 2024, 00:00 UTC, in seconds since the epoch.
const SECOND_HALF_S: i64 = 1_705_363_200;

fn main() -> ExitCode {
    moruna_gate_tasks::serve(grows)
}

/// `audit`, holding [`GROWN_BYTES`] more while it answers a batch of the second half of the month.
fn grows(batch: &RecordBatch) -> moruna_gate_functions::error::Result<RecordBatch> {
    let held = second_half(batch)?.then(|| {
        let mut held = vec![0u8; GROWN_BYTES];
        for page in held.chunks_mut(4096) {
            page[0] = 1;
        }
        held
    });
    let answer = moruna_gate_functions::batch::audit(batch);
    drop(std::hint::black_box(held));
    answer
}

/// Whether any trip of `batch` was picked up on or after 16 January 2024. The pickup column's unit
/// is read from its values' size (seconds of this century are ten digits, and each finer unit
/// three more), so a timestamp of any unit and the normalized data's integers are read alike.
fn second_half(batch: &RecordBatch) -> moruna_gate_functions::error::Result<bool> {
    let pickup = moruna_gate_functions::batch::ints(batch, "pickup_at")?;
    Ok((0..pickup.len()).any(|i| {
        pickup.get(i).is_some_and(|at| {
            let per_second: i64 = match at.unsigned_abs() {
                0..100_000_000_000 => 1,
                100_000_000_000..100_000_000_000_000 => 1_000,
                100_000_000_000_000..100_000_000_000_000_000 => 1_000_000,
                _ => 1_000_000_000,
            };
            at >= SECOND_HALF_S.saturating_mul(per_second)
        })
    }))
}
