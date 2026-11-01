//! The record check, where it runs (SDD 29 f.3): in the engine's process, on a record no
//! task can write any more, before anything else of the engine is told of it.
//!
//! `batch_put` has already unmapped the window from the task when the record reaches
//! here, so the bytes checked are the bytes every consumer will read. Only the framing is
//! read (its two messages, a page or so); the body is checked by its buffers' offsets and
//! lengths against the region the task handed back, and is never read or copied.

use alloc::vec::Vec;

use moruna_ipc_check::record::{self, FRAMING_MAX, Need, Record};

/// The bytes read first: a framing is nearly always within one page.
const FIRST: usize = 4096;

/// Checks the record of `len` bytes that `read` reads (an offset into the record and a
/// buffer to fill): the framing is read, then `check_record` judges it. The refusal is the
/// sentence the task is ended with.
pub fn verify(
    len: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), moruna_abi::Error>,
) -> Result<Record, alloc::string::String> {
    let have = usize::try_from(len.min(FIRST as u64)).unwrap_or(FIRST);
    let mut framing = Vec::new();
    framing
        .try_reserve_exact(have)
        .map_err(|_| alloc::string::String::from("no memory to read the record's framing"))?;
    framing.resize(have, 0);
    read(0, &mut framing)
        .map_err(|e| alloc::format!("the record could not be read: {}", e.name()))?;
    loop {
        match record::framing_need(&framing)? {
            Need::Framing(end) => {
                if end as u64 > len {
                    return Err(alloc::format!(
                        "the record's framing reaches byte {end}, past the {len} bytes put"
                    ));
                }
                if end > framing.len() {
                    let from = framing.len();
                    grow(&mut framing, end)?;
                    read(from as u64, &mut framing[from..]).map_err(|e| {
                        alloc::format!("the record could not be read: {}", e.name())
                    })?;
                }
                framing.truncate(end);
                return record::check_record(&framing, len);
            }
            Need::More(n) => {
                if n as u64 > len || n > FRAMING_MAX + 8 {
                    return Err(alloc::format!(
                        "the record ends at byte {len}, inside its framing (which needs {n})"
                    ));
                }
                if n <= framing.len() {
                    return Err(alloc::string::String::from(
                        "the record's framing could not be measured",
                    ));
                }
                let from = framing.len();
                grow(&mut framing, n)?;
                read(from as u64, &mut framing[from..])
                    .map_err(|e| alloc::format!("the record could not be read: {}", e.name()))?;
            }
        }
    }
}

fn grow(framing: &mut Vec<u8>, to: usize) -> Result<(), alloc::string::String> {
    framing
        .try_reserve_exact(to.saturating_sub(framing.len()))
        .map_err(|_| alloc::string::String::from("no memory to read the record's framing"))?;
    framing.resize(to, 0);
    Ok(())
}
