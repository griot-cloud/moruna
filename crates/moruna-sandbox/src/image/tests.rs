//! IM-T26 and IM-T27 (SDD 19 k): the stream read with care onto the plane, and the words of a
//! failure.

use std::io::Cursor;
use std::sync::Arc;

use moruna_kernel::arrow::array::{DictionaryArray, Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Schema};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;

use moruna_plane_linux::stream::Reader;

use super::*;

fn batch(n: i64) -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, false),
            Field::new("s", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from_iter_values(0..n)),
            Arc::new(StringArray::from_iter_values(
                (0..n).map(|i| format!("row {i}")),
            )),
        ],
    )
    .unwrap()
}

/// A stream of `batches` batches, with the end-of-stream marker when `end`.
fn stream(batches: usize, end: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &batch(1).schema()).unwrap();
    for i in 0..batches {
        writer.write(&batch(i as i64 + 2)).unwrap();
    }
    if end {
        writer.finish().unwrap();
    } else {
        drop(writer);
    }
    out
}

/// Every record batch of `bytes`, each laid as a record into a window and checked as the
/// engine checks a put record; or the first refusal.
fn records(bytes: Vec<u8>) -> std::result::Result<Vec<u64>, String> {
    let mut reader = Reader::new(Cursor::new(bytes));
    let mut rows = Vec::new();
    while let Some(pending) = reader.batch()? {
        let mut window = vec![0u8; pending.len() as usize];
        let len = reader.lay(pending, &mut window)?;
        let n = match moruna_ipc_check::record::framing_need(&window)? {
            moruna_ipc_check::record::Need::Framing(n) => n,
            other => return Err(format!("{other:?}")),
        };
        rows.push(moruna_ipc_check::record::check_record(&window[..n], len)?.rows);
    }
    Ok(rows)
}

/// IM-T26 the_output_is_read_with_care: the reader of f.6 lays each record batch of a stream as
/// a record the engine's check passes; the marker and the pipe closed at a boundary end it; a
/// declared length over the bound, a stream that ends inside a message, a dictionary and bytes
/// that are no message are refused with their reason.
#[test]
fn im_t26_the_output_is_read_with_care() {
    assert_eq!(records(stream(2, true)).unwrap(), vec![2, 3]);
    assert_eq!(
        records(stream(1, false)).unwrap(),
        vec![2],
        "closed at a boundary"
    );
    assert_eq!(
        records(Vec::new()).unwrap(),
        Vec::<u64>::new(),
        "nothing at all"
    );
    let whole = stream(1, true);
    for cut in [whole.len() - 12, whole.len() - 200, 6, 2] {
        let e = records(whole[..cut].to_vec()).unwrap_err();
        assert!(
            e.contains("ends inside") || e.contains("ends after"),
            "{cut}: {e}"
        );
    }
    let e = records(b"this is not an Arrow stream\n".to_vec()).unwrap_err();
    assert!(
        e.contains("more than the") || e.contains("not an Arrow"),
        "{e}"
    );
    let e = records(vec![0xff; 4]).unwrap_err();
    assert!(e.contains("continuation"), "{e}");
    let mut hostile = vec![0xff; 4];
    hostile.extend_from_slice(&u32::MAX.to_le_bytes());
    let e = records(hostile).unwrap_err();
    assert!(e.contains("one may hold"), "{e}");
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&8u32.to_le_bytes());
    hostile.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(records(hostile).is_err());
    // A dictionary column: refused as the plane refuses it, before any batch.
    let d: DictionaryArray<Int32Type> = vec!["a", "b", "a"].into_iter().collect();
    let dict = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "d",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            false,
        )])),
        vec![Arc::new(d)],
    )
    .unwrap();
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &dict.schema()).unwrap();
    writer.write(&dict).unwrap();
    writer.finish().unwrap();
    let e = records(out).unwrap_err();
    assert!(e.contains("carries no dictionaries"), "{e}");
    // A window too small for the record is refused, not overrun.
    let mut reader = Reader::new(Cursor::new(stream(1, true)));
    let pending = reader.batch().unwrap().unwrap();
    let mut small = vec![0u8; 4096];
    let e = reader.lay(pending, &mut small).unwrap_err();
    assert!(e.contains("more than the 4096 bytes of the window"), "{e}");
}

/// IM-T27 the_words_of_a_failure.
#[test]
fn im_t27_the_words_of_a_failure() {
    // Image Launch owns and tests user, environment and command-path interpretation.
    // How a process ends.
    let name = "scorer:1";
    assert_eq!(
        describe(Exit::Code(3), false, name),
        "image `scorer:1` exited with status 3"
    );
    let sys = describe(Exit::Signal(libc::SIGSYS), false, name);
    assert!(
        sys.contains("made a system call it is not allowed"),
        "{sys}"
    );
    assert!(describe(Exit::Signal(libc::SIGKILL), true, name).contains("more memory than"));
    assert!(describe(Exit::Signal(libc::SIGKILL), false, name).contains("SIGKILL (9)"));
    assert!(describe(Exit::Signal(libc::SIGSEGV), false, name).contains("SIGSEGV"));
    assert!(describe(Exit::Signal(63), false, name).contains("a signal (63)"));
    assert!(describe(Exit::Lost, false, name).contains("status is not known"));

    // Its standard error, last lines first quoted in order and no more than twenty.
    assert!(with_stderr("head".into(), &[]).ends_with("it wrote nothing to its standard error"));
    let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let quoted = with_stderr("head".into(), &lines);
    assert!(quoted.starts_with("head; the last 20 lines of its standard error:\n  line 10\n"));
    assert!(quoted.ends_with("  line 29"));
    assert_eq!(quoted.lines().count(), 21);
}

/// The real parser/refusal cause survives an observed exit, including successful
/// exits and the SIGKILL used to stop a malformed-output process.
#[test]
fn protocol_refusal_is_retained_alongside_actual_process_exit() {
    let invalid = "its output is not an Arrow IPC stream: invalid continuation marker";
    let head = failure_head("echo", invalid, Some(Exit::Code(0)), false);
    assert!(head.contains("exited with status 0"), "{head}");
    assert!(head.contains(invalid), "{head}");

    let refused = "its output was refused by the engine's check: invalid continuation marker";
    let head = failure_head("echo", refused, Some(Exit::Signal(libc::SIGKILL)), false);
    assert!(head.contains("SIGKILL (9)"), "{head}");
    assert!(head.contains(refused), "{head}");
    assert_eq!(
        failure_head("echo", refused, None, false),
        format!("image `echo`: {refused}")
    );
}
