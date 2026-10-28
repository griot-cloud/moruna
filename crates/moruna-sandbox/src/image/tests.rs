//! IM-T26 and IM-T27 (SDD 19 k): the stream read with care, and the words of a failure.

use std::io::Cursor;
use std::sync::Arc;

use moruna_kernel::arrow::array::{Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;

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

/// IM-T26 the_output_is_read_with_care.
#[test]
fn im_t26_the_output_is_read_with_care() {
    let limit = 1 << 20;
    // Batches, then the marker; then the end again, however often it is asked.
    let mut output = Output::new(Cursor::new(stream(2, true)));
    for rows in [2, 3] {
        match output.next(limit).unwrap() {
            Next::Batch(b) => assert_eq!(b.num_rows(), rows),
            Next::End => panic!("a batch was expected"),
        }
    }
    assert!(matches!(output.next(limit).unwrap(), Next::End));
    // No marker, the pipe closed at a message boundary: the same end.
    let mut output = Output::new(Cursor::new(stream(1, false)));
    assert!(matches!(output.next(limit).unwrap(), Next::Batch(_)));
    assert!(matches!(output.next(limit).unwrap(), Next::End));
    // Nothing at all: the end.
    assert!(matches!(
        Output::new(Cursor::new(Vec::new())).next(limit).unwrap(),
        Next::End
    ));
    // A stream that ends inside a message, a body and a length.
    let whole = stream(1, true);
    for cut in [whole.len() - 12, whole.len() - 200, 6, 2] {
        let mut output = Output::new(Cursor::new(whole[..cut].to_vec()));
        let mut failed = None;
        for _ in 0..3 {
            match output.next(limit) {
                Err(e) => {
                    failed = Some(e);
                    break;
                }
                Ok(Next::End) => break,
                Ok(Next::Batch(_)) => {}
            }
        }
        let e = failed.unwrap_or_else(|| panic!("a stream cut at {cut} is refused"));
        assert!(
            e.contains("ends inside") || e.contains("not an Arrow"),
            "{cut}: {e}"
        );
    }
    // A declared length over the limit is refused before it is read; bytes that are no message
    // are refused with their reason; a continuation marker with nothing after it is refused.
    let e = Output::new(Cursor::new(stream(1, true)))
        .next(16)
        .unwrap_err();
    assert!(e.contains("more than the 16 one may hold"), "{e}");
    let e = Output::new(Cursor::new(b"this is not an Arrow stream\n".to_vec()))
        .next(limit)
        .unwrap_err();
    assert!(!e.is_empty(), "garbage is refused");
    let e = Output::new(Cursor::new(vec![0xff; 4]))
        .next(limit)
        .unwrap_err();
    assert!(e.contains("continuation"), "{e}");
    let mut hostile = vec![0xff; 4];
    hostile.extend_from_slice(&u32::MAX.to_le_bytes());
    let e = Output::new(Cursor::new(hostile)).next(limit).unwrap_err();
    assert!(e.contains("one may hold"), "{e}");
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&8u32.to_le_bytes());
    hostile.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let e = Output::new(Cursor::new(hostile)).next(limit).unwrap_err();
    assert!(
        e.contains("not an Arrow IPC message") || e.contains("message"),
        "{e}"
    );
}

/// IM-T27 the_words_of_a_failure.
#[test]
fn im_t27_the_words_of_a_failure() {
    // The user.
    assert_eq!(identity("").unwrap(), (0, 0));
    assert_eq!(identity("root").unwrap(), (0, 0));
    assert_eq!(identity("1000").unwrap(), (1000, 1000));
    assert_eq!(identity("1000:50").unwrap(), (1000, 50));
    let e = identity("app").unwrap_err().to_string();
    assert!(e.contains("user `app` is not numeric"), "{e}");
    assert!(identity("1000:staff").is_err());

    // The environment: the image's, then the settings' replacing and adding; nothing else.
    let image = vec![
        "PATH=/bin".to_string(),
        "A=1".to_string(),
        "B=x=y".to_string(),
    ];
    let mut settings = BTreeMap::new();
    settings.insert("A".to_string(), "2".to_string());
    settings.insert("C".to_string(), "3".to_string());
    assert_eq!(
        environment(&image, &settings).unwrap(),
        [("PATH", "/bin"), ("A", "2"), ("B", "x=y"), ("C", "3")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
    );
    let e = environment(&["NOEQUALS".to_string()], &settings)
        .unwrap_err()
        .to_string();
    assert!(e.contains("`NOEQUALS` is not NAME=value"), "{e}");

    // The paths a command is tried at.
    let paths = |c: &str, p: &str| -> Vec<String> {
        candidates(c, p)
            .unwrap()
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(paths("/bin/x", "/a"), ["/bin/x"]);
    assert_eq!(paths("./x", "/a"), ["./x"]);
    assert_eq!(paths("x", "/a::/b"), ["/a/x", "./x", "/b/x"]);
    assert!(candidates("a\0b", "/a").is_err());

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
