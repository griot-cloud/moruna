//! Item 5.0's proof (system design 5A0): an ordinary Rust program with `std`, using arrow-rs,
//! built for Moruna with `kernel/tools/build-std.sh` and run as a native task.
//!
//! It makes Arrow arrays, runs compute kernels on them (a sum, a filter, a sort), writes a
//! batch of them as an Arrow IPC stream in memory and reads it back, and has two threads add to
//! one `Mutex`. Each result is checked, so a wrong one panics, and printed: stdout is the task's
//! log on Moruna.

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread;

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

fn main() -> Result<(), ArrowError> {
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
    let names: Vec<_> = read[0].column(0).as_string::<i32>().iter().flatten().collect();
    println!(
        "std-arrow: IPC stream of {} bytes reads back {} rows: {names:?}",
        stream.len(),
        read[0].num_rows()
    );

    // Threads: two of them add to one vector under one Mutex.
    let shared = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..2u64)
        .map(|t| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for i in 0..1_000 {
                    shared.lock().expect("no thread panicked").push(t * 1_000 + i);
                }
                t
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("the thread ran to its end");
    }
    let mut pushed = shared.lock().expect("no thread panicked").clone();
    pushed.sort_unstable();
    assert_eq!(pushed, (0..2_000).collect::<Vec<u64>>());
    println!("std-arrow: two threads pushed {} values under one Mutex", pushed.len());

    println!("std-arrow: ok");
    Ok(())
}
