// SPDX-License-Identifier: MPL-2.0

//! Host tests of the server's decisions.

use alloc::{string::ToString, vec};

use super::*;

#[test]
fn process_names_follow_the_old_server() {
    assert_eq!(process_name(b"/bin/Hello World"), "lx-hello-world");
    assert_eq!(process_name(b"/usr/bin/"), "lx-program");
    assert_eq!(process_name(b"engine.v2_x-y"), "lx-engine.v2_x-y");
    let long = process_name(b"/a/abcdefghijklmnopqrstuvwxyz0123456789");
    assert_eq!(long, "lx-abcdefghijklmnopqrstuvwxyz012");
}

#[test]
fn requests_must_be_absolute_short_and_without_zero_bytes() {
    let none: [Vec<u8>; 0] = [];
    assert!(request_ok(
        b"/bin/true",
        &[b"true".to_vec()],
        &[b"A=1".to_vec()]
    ));
    assert!(!request_ok(b"bin/true", &none, &none));
    assert!(!request_ok(b"", &none, &none));
    assert!(!request_ok(b"/bin/t\0rue", &none, &none));
    assert!(!request_ok(b"/bin/true", &[b"a\0".to_vec()], &none));
    assert!(!request_ok(b"/bin/true", &none, &[b"A=\0".to_vec()]));
    let big = vec![b'a'; REQUEST_MAX];
    assert!(!request_ok(b"/bin/true", &[big], &none));
}

#[test]
fn output_keeps_its_first_bytes_and_counts_the_rest() {
    let mut output = Output::default();
    output.line(b"hello");
    assert_eq!(output.bytes, b"hello\n");
    let long = vec![b'x'; OUTPUT_MAX];
    output.line(&long);
    assert_eq!(output.bytes.len(), OUTPUT_MAX);
    assert_eq!(output.dropped, 7);
}

#[test]
fn ring_lines_are_read_in_order_with_their_streams() {
    let mut region = vec![0u8; 64 * 1024];
    {
        let ring = Ring::init(&mut region).unwrap();
        let producer = ring.producer();
        let out = [(log::STREAM, log::STDOUT)];
        let err = [(log::STREAM, log::STDERR)];
        producer.push_record(1, 0, 1, "lx-a", &out, b"one").unwrap();
        producer.push_record(2, 0, 2, "lx-a", &err, b"two").unwrap();
        producer
            .push_record(3, 0, 1, "lx-a", &[], b"three")
            .unwrap();
    }
    let lines = ring_lines(&mut region);
    let want = [("stdout", "one"), ("stderr", "two"), ("stdout", "three")];
    assert_eq!(lines.len(), want.len());
    for ((stream, line), (s, l)) in lines.iter().zip(want) {
        assert_eq!((stream.as_str(), &line[..]), (s, l.as_bytes()));
    }
    // Read once: the next read finds nothing new.
    assert!(ring_lines(&mut region).is_empty());
    // Not a ring: nothing.
    assert!(ring_lines(&mut [0u8; 64]).is_empty());
}

#[test]
fn a_report_makes_the_answer() {
    let output = || {
        let mut o = Output::default();
        o.line(b"hi");
        o
    };
    let exited = Report {
        how: report::EXITED,
        code: 7,
        served: 40,
        refused: vec![(334, 1)],
    };
    let e = ended(Some(exited), output()).unwrap();
    assert_eq!((e.how, e.code, e.served), (how::EXITED, 7, 40));
    assert_eq!((e.output.0, e.refused), (b"hi\n".to_vec(), vec![(334, 1)]));
    let signaled = Report {
        how: report::SIGNALED,
        code: 9,
        served: 1,
        refused: vec![],
    };
    assert_eq!(ended(Some(signaled), output()).unwrap().how, how::SIGNALED);
    let odd = Report {
        how: 0x7f,
        code: 0,
        served: 0,
        refused: vec![],
    };
    assert_eq!(ended(Some(odd), output()).unwrap().how, how::FAULTED);
    let faulted = ended(None, output()).unwrap();
    assert_eq!((faulted.how, faulted.code), (how::FAULTED, -1));
    assert_eq!(faulted.output.0, b"hi\n");
}

#[test]
fn a_program_that_never_started_is_refused_by_its_error() {
    let not_started = |errno| Report {
        how: report::NOT_STARTED,
        code: errno,
        served: 0,
        refused: vec![],
    };
    let error = |errno| ended(Some(not_started(errno)), Output::default()).unwrap_err();
    assert_eq!(error(2), linux::Error::NotFound);
    assert_eq!(error(20), linux::Error::NotFound);
    assert_eq!(error(8), linux::Error::NotExecutable);
    assert_eq!(error(13), linux::Error::NotExecutable);
    assert_eq!(error(12), linux::Error::NoProcess);
}

#[test]
fn echoed_lines_name_their_program() {
    assert_eq!(echo("lx-hello", b"hi there"), "linux lx-hello: hi there");
    assert_eq!(echo("lx-x", &[0xff]).to_string(), "linux lx-x: \u{fffd}");
}
