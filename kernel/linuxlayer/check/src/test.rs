// SPDX-License-Identifier: MPL-2.0

use alloc::{string::ToString, vec};

use linux_protocol::Bytes;

use super::*;

fn ended(how: u8, code: i64, text: &str) -> Ended {
    Ended {
        how,
        code,
        output: Bytes(text.as_bytes().to_vec()),
        dropped: 0,
        refused: vec![],
        served: 1,
    }
}

const ALL: &str = "taskcheck: image: ok: a\ntaskcheck: proc: ok: b\n\
                   taskcheck: tmp: ok: c\ntaskcheck: epoll: ok: d\n\
                   taskcheck: descriptor 2 reached the server\n";

#[test]
fn every_check_and_the_stderr_line_pass() {
    let line = verdict(&ended(how::EXITED, 0, ALL)).unwrap();
    assert!(line.starts_with("image: a; proc: b; tmp: c; epoll: d; descriptor 2"));
}

#[test]
fn a_failed_task_says_how_it_ended() {
    let why = verdict(&ended(how::EXITED, 1, "taskcheck: FAIL: x\n")).unwrap_err();
    assert!(why.contains("ended 1 with code 1"), "{why}");
    assert!(why.contains("taskcheck: FAIL: x"), "{why}");
    let why = verdict(&ended(how::FAULTED, -1, "")).unwrap_err();
    assert!(why.contains("ended 3"), "{why}");
}

#[test]
fn a_missing_check_or_stderr_line_fails() {
    let no_tmp = ALL.replace("taskcheck: tmp: ok: c\n", "");
    let why = verdict(&ended(how::EXITED, 0, &no_tmp)).unwrap_err();
    assert!(why.contains("no `tmp` line"), "{why}");
    let no_err = ALL.replace(STDERR_LINE, "");
    let why = verdict(&ended(how::EXITED, 0, &no_err)).unwrap_err();
    assert!(why.contains("descriptor 2"), "{why}");
}

#[test]
fn the_arguments_name_the_program_and_its_limits() {
    let argv = argv();
    assert_eq!(argv[0], PROGRAM);
    assert_eq!(argv[1], (LIMIT / 1024).to_string());
    assert_eq!(argv[2], SCRATCH.to_string());
}
