// SPDX-License-Identifier: MPL-2.0

//! Host tests of the runner, with stand-in test binaries (host shell scripts: the image
//! has no shell, but these run on the host only).

use std::{os::unix::fs::PermissionsExt, process::Command};

use super::*;

/// A fresh directory for one test.
fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gvisor-runner-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in test binary: a script.
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn names_skip_comments_and_blanks() {
    assert_eq!(
        names("# head\npipe_test\n\n  dup_test # why\n#gone_test\n"),
        ["pipe_test", "dup_test"]
    );
    assert!(names("").is_empty());
}

#[test]
fn the_filter_leaves_out_every_blocked_case() {
    assert_eq!(filter(""), "*");
    assert_eq!(filter("# only a reason\n"), "*");
    assert_eq!(
        filter("# reason\nPipes/PipeTest.*/smallbuffer\nPipeTest.Flags\n"),
        "-Pipes/PipeTest.*/smallbuffer:PipeTest.Flags"
    );
}

#[test]
fn verdicts_say_how_a_test_ended() {
    let status = |cmd: &str| Command::new("/bin/sh").arg("-c").arg(cmd).status();
    assert_eq!(
        verdict("a_test", status("exit 0")),
        (true, "gvisor: ok: a_test".into())
    );
    assert_eq!(
        verdict("a_test", status("exit 3")),
        (false, "gvisor: FAIL: a_test exited with 3".into())
    );
    assert_eq!(
        verdict("a_test", status("kill -9 $$")),
        (false, "gvisor: FAIL: a_test was killed by signal 9".into())
    );
    let missing = Command::new("/nonexistent/a_test").status();
    assert!(
        verdict("a_test", missing)
            .1
            .starts_with("gvisor: FAIL: a_test could not run:")
    );
}

#[test]
fn run_runs_each_named_test_with_its_filter_in_an_empty_scratch() {
    let root = dir("run");
    let scratch = root.join("scratch");
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::create_dir_all(root.join("blocklists")).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    fs::write(
        root.join("applies"),
        "# tests\npass_test\nfail_test\nmissing_test\n",
    )
    .unwrap();
    fs::write(
        root.join("blocklists/pass_test"),
        "# why\nSuite.Case\nOther.*\n",
    )
    .unwrap();
    let seen = root.join("seen");
    // It records its filter and its scratch, leaves a file there, and passes.
    script(
        &root.join("tests/pass_test"),
        &format!(
            "echo \"$1 $TEST_TMPDIR\" > {}; touch \"$TEST_TMPDIR/left\"; mkdir \"$TEST_TMPDIR/d\"",
            seen.display()
        ),
    );
    script(
        &root.join("tests/fail_test"),
        "test -e \"$TEST_TMPDIR/left\" && exit 9; exit 1",
    );
    let mut lines = Vec::new();
    let (passed, ran) = run(&root, &scratch, &mut |line| lines.push(line.to_string()));
    assert_eq!((passed, ran), (1, 3));
    assert_eq!(
        fs::read_to_string(&seen).unwrap().trim(),
        format!("--gtest_filter=-Suite.Case:Other.* {}", scratch.display())
    );
    assert!(lines.contains(&"gvisor: ok: pass_test".to_string()));
    // The scratch was emptied before the next test: it saw no file left.
    assert!(lines.contains(&"gvisor: FAIL: fail_test exited with 1".to_string()));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("gvisor: FAIL: missing_test could not run"))
    );
    assert_eq!(lines.last().unwrap(), "gvisor: 1 of 3 test binaries passed");
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn nothing_named_runs_nothing() {
    let root = dir("none");
    let mut lines = Vec::new();
    assert_eq!(
        run(&root, &root, &mut |l| lines.push(l.to_string())),
        (0, 0)
    );
    assert_eq!(lines, ["gvisor: 0 of 0 test binaries passed"]);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_descriptor_limit_rises_or_says_why_not() {
    // A host may cap it lower; either way the call answers.
    let _ = raise_nofile();
    reap();
}

#[test]
fn start_passes_only_when_every_test_passed() {
    let root = dir("start");
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("applies"), "a_test\n").unwrap();
    script(&root.join("tests/a_test"), "exit 0");
    let scratch = root.join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let mut lines = Vec::new();
    let root_name = Some(root.display().to_string());
    assert!(start(root_name.clone(), &scratch, &mut |l| lines
        .push(l.to_string())));
    script(&root.join("tests/a_test"), "exit 1");
    assert!(!start(root_name, &scratch, &mut |l| lines.push(l.to_string())));
    // No root given: the image's, absent on the host, so nothing ran.
    assert!(!start(None, &scratch, &mut |l| lines.push(l.to_string())));
    assert_eq!(lines.last().unwrap(), "gvisor: 0 of 0 test binaries passed");
    fs::remove_dir_all(&root).unwrap();
}
