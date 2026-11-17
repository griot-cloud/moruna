//! Item 5.0's proof (system design 5A0): an ordinary Rust program with `std`, using arrow-rs,
//! built for Moruna with `kernel/tools/build-std.sh` and run as a native task.
//!
//! It makes Arrow arrays, runs compute kernels on them (a sum, a filter, a sort), writes a
//! batch of them as an Arrow IPC stream in memory and reads it back, and has two threads add to
//! one `Mutex`. It also takes what std on Moruna gives it: its name from the start page, a
//! spawned thread's 2 MiB stack, the wall clock, and getrandom's bytes from the core (through
//! `kernel/getrandom`, over `std::os::moruna`). Each result is checked, so a wrong one panics, and printed, on the task's log or
//! its console. Every line it prints is the same on every run, for the boot's check.

use std::hint::black_box;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrow_arith::aggregate::sum;
use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_ord::cmp::gt;
use arrow_ord::sort::sort;
use arrow_schema::{ArrowError, DataType, Field, Schema};
use arrow_select::filter::{filter, filter_record_batch};
// getrandom's custom backend on Moruna, the core's random_read (build-std.sh selects it).
use moruna_getrandom as _;

/// 2025-01-01T00:00:00Z, which any wall clock set by now is past.
const YEAR_2025: Duration = Duration::from_secs(1_735_689_600);

/// Recurses `depth` times through a frame of 16 KiB: 1 MiB at 64.
fn deep(depth: u32) -> u64 {
    let frame = black_box([depth as u8; 16 << 10]);
    if depth == 0 {
        return u64::from(frame[0]);
    }
    deep(depth - 1) + u64::from(frame[frame.len() - 1])
}

fn main() -> Result<(), ArrowError> {
    // Its name, the one argument the start page gives it.
    let name = std::env::args().next().expect("the process's name");
    println!("std-arrow: started as {name}");

    let values = Int64Array::from(vec![7, 3, 9, 1, 5, 8, 2, 6, 4, 10]);
    let names = StringArray::from(vec!["g", "c", "i", "a", "e", "h", "b", "f", "d", "j"]);

    // Compute: a sum, a filter and a sort.
    let total = sum(&values).expect("ten values have a sum");
    assert_eq!(total, 55);
    println!("std-arrow: sum of {} values is {total}", values.len());

    let above = gt(&values, &Int64Array::new_scalar(5))?;
    let kept = filter(&values, &above)?;
    let kept = kept.as_primitive::<Int64Type>().values();
    assert_eq!(kept.as_ref(), [7, 9, 8, 6, 10]);
    println!("std-arrow: filter value > 5 keeps {kept:?}");

    let sorted = sort(&values, None)?;
    let sorted = sorted.as_primitive::<Int64Type>().values();
    assert_eq!(sorted.as_ref(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    println!("std-arrow: sort gives {sorted:?}");

    // IPC: a batch written as a stream into memory, and read back.
    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("value", DataType::Int64, false),
    ]));
    let columns: Vec<ArrayRef> = vec![Arc::new(names), Arc::new(values)];
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns)?;
    let batch = filter_record_batch(&batch, &above)?;
    let mut stream = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut stream, &schema)?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    let read: Vec<RecordBatch> =
        StreamReader::try_new(Cursor::new(&stream), None)?.collect::<Result<_, _>>()?;
    assert_eq!(read, [batch.clone()]);
    let names: Vec<_> = read[0]
        .column(0)
        .as_string::<i32>()
        .iter()
        .flatten()
        .collect();
    println!(
        "std-arrow: an IPC stream in memory reads back {} rows: {names:?}",
        read[0].num_rows()
    );

    // Threads: two of them add to one vector under one Mutex, yielding as they go.
    let shared = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..2u64)
        .map(|t| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for i in 0..1_000 {
                    shared
                        .lock()
                        .expect("no thread panicked")
                        .push(t * 1_000 + i);
                    if i % 100 == 0 {
                        thread::yield_now();
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("the thread ran to its end");
    }
    let mut pushed = shared.lock().expect("no thread panicked").clone();
    pushed.sort_unstable();
    assert_eq!(pushed, (0..2_000).collect::<Vec<u64>>());
    println!(
        "std-arrow: two threads pushed {} values under one Mutex",
        pushed.len()
    );

    // A spawned thread's default stack: 2 MiB, of which this uses 1 MiB.
    let deepest = thread::spawn(|| deep(64)).join().expect("the stack held");
    assert_eq!(deepest, (1..=64).sum::<u64>());
    println!("std-arrow: a thread recursed through 1 MiB of its stack");

    // The wall clock: the core's, from the clock page.
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970");
    assert!(
        since > YEAR_2025,
        "the wall clock reads {since:?} after 1970"
    );
    println!("std-arrow: the wall clock reads a time after 2025");

    // getrandom, as arrow-rs's hasher reaches it: the core's random bytes.
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the core gives random bytes");
    assert!(
        bytes.iter().any(|b| *b != 0),
        "32 zero bytes are not random"
    );
    println!(
        "std-arrow: getrandom gave {} bytes from the core",
        bytes.len()
    );

    println!("std-arrow: ok");
    Ok(())
}
