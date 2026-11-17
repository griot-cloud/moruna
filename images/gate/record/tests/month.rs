//! A whole month through the native path on the host, morsel by morsel, as a task meets it: each
//! batch of `GATE_MONTH_ROWS` rows made into a data-plane record, the job answered in parts into
//! windows of `GATE_MONTH_WINDOW` bytes, every part checked by the engine's record check and the
//! rows counted against the arrow-rs port's. It runs only when `GATE_MONTH_IPC` names an Arrow
//! IPC file of the month (bench/vm/normalize.py's schema), so a failing morsel on the VM can be
//! found here by its index; `GATE_MONTH_JOBS` picks the jobs (all four NYC jobs by default).

use std::fs::File;

use arrow::array::RecordBatch;
use arrow::ipc::reader::{FileReader, StreamReader};
use arrow::ipc::writer::StreamWriter;
use moruna_gate_record::Job;
use moruna_gate_record::parts::{Carry, Wrote, apply_part};
use moruna_ipc_check::frame::{END_OF_STREAM, body_length, message_len};
use moruna_ipc_check::record::{Need, check_record, framing_need, reframe};

fn aligned(n: usize) -> Vec<u64> {
    vec![0u64; n.div_ceil(8)]
}

/// `batch` as a data-plane record, in an 8-aligned buffer.
fn record_of(batch: &RecordBatch) -> (Vec<u64>, usize) {
    let mut stream = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut stream, &batch.schema()).unwrap();
        w.write(batch).unwrap();
        w.finish().unwrap();
    }
    let l1 = message_len(&stream).unwrap();
    let schema = &stream[..8 + l1];
    let l2 = message_len(&stream[8 + l1..]).unwrap();
    let rb = &stream[8 + l1..8 + l1 + 8 + l2];
    let body_at = 8 + l1 + 8 + l2;
    let body_len = body_length(rb).unwrap() as usize;
    let body = &stream[body_at..body_at + body_len];
    let r = reframe(schema, rb).unwrap();
    let mut buf = aligned(r.record_len as usize);
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut buf);
    bytes[..r.framing.len()].copy_from_slice(&r.framing);
    for (from, to, len) in r.moves {
        bytes[to as usize..(to + len) as usize]
            .copy_from_slice(&body[from as usize..(from + len) as usize]);
    }
    (buf, r.record_len as usize)
}

/// A part's rows, after the engine's record check passed it.
fn rows_of(record: &[u8]) -> usize {
    let Ok(Need::Framing(end)) = framing_need(record) else {
        panic!("no framing")
    };
    check_record(&record[..end], record.len() as u64).expect("the engine's record check");
    let mut stream = record.to_vec();
    stream.extend_from_slice(&END_OF_STREAM);
    StreamReader::try_new(&stream[..], None)
        .unwrap()
        .map(|b| b.unwrap().num_rows())
        .sum()
}

#[test]
fn a_month_morsel_by_morsel() {
    let Some(path) = std::env::var_os("GATE_MONTH_IPC") else {
        eprintln!("GATE_MONTH_IPC is not set: skipped");
        return;
    };
    let env = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let rows = env("GATE_MONTH_ROWS", 87_000);
    let window_bytes = env("GATE_MONTH_WINDOW", 16 << 20);
    let jobs =
        std::env::var("GATE_MONTH_JOBS").unwrap_or_else(|_| "enrich,score,occupancy,audit".into());
    let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let all: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    let month = arrow::compute::concat_batches(&schema, &all).unwrap();
    let mut failed = Vec::new();
    for name in jobs.split(',') {
        let job = Job::by_name(name).unwrap();
        let port = moruna_gate_functions::batch::by_name(name).unwrap();
        let (mut parts_max, mut out_rows, mut largest) = (0u32, 0usize, 0usize);
        for (morsel, start) in (0..month.num_rows()).step_by(rows).enumerate() {
            let batch = month.slice(start, rows.min(month.num_rows() - start));
            let want = port(&batch);
            let (buf, len) = record_of(&batch);
            let record = &bytemuck::cast_slice::<u64, u8>(&buf)[..len];
            let mut window = aligned(window_bytes);
            let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
            let mut carry = Carry::new();
            let (mut part, mut got) = (0u32, 0usize);
            let outcome = loop {
                match apply_part(job, record, w, part, &mut carry) {
                    Ok(Wrote::Last(n)) => {
                        got += rows_of(&w[..n]);
                        largest = largest.max(n);
                        break Ok(());
                    }
                    Ok(Wrote::More(n)) => {
                        got += rows_of(&w[..n]);
                        largest = largest.max(n);
                        part += 1;
                    }
                    Err(e) => break Err(e),
                }
            };
            match (outcome, want) {
                (Ok(()), Ok(want)) if got == want.num_rows() => out_rows += got,
                (Ok(()), Ok(want)) => failed.push(format!(
                    "{name} morsel {morsel}: {got} rows, the port {}",
                    want.num_rows()
                )),
                (Err(e), Err(_)) => eprintln!("{name} morsel {morsel}: refused, as the port: {e}"),
                (outcome, want) => failed.push(format!(
                    "{name} morsel {morsel} ({} rows from row {start}, a record of {len} bytes): \
                     the task {outcome:?}, the port {}",
                    batch.num_rows(),
                    want.map(|b| format!("{} rows", b.num_rows()))
                        .unwrap_or_else(|e| format!("{e:?}"))
                )),
            }
            parts_max = parts_max.max(part + 1);
        }
        eprintln!(
            "{name}: {} morsels of {rows} rows, {out_rows} rows out, at most {parts_max} parts in \
             windows of {window_bytes} bytes, the largest part {largest} bytes",
            month.num_rows().div_ceil(rows)
        );
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
