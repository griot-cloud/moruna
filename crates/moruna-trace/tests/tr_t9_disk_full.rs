//! TR-T9 disk_full. The overflow file cannot be written; the records are still counted and
//! `overflow_failed` is set. Proves the failure path of section h.
//!
//! The SDD puts the overflow directory on a 1 MiB tmpfs. A 1 MiB filesystem cannot be
//! mounted on the development host (macOS arm64) without administrator rights, and the test
//! must run on the Linux CI host as well, so the condition is simulated at the same boundary
//! a full disk reaches: the writer's attempt to create the overflow file fails. Two
//! independent simulations are used, each with a different errno, and each must produce the
//! same behaviour. Neither relies on file permissions, because the container gate runs as
//! root, for which a directory that refuses writes refuses nothing.

mod common;

use moruna_kernel::TraceSink;
use moruna_trace::TraceWriter;
use common::{TempDir, config, record};

const RECORDS: u64 = 30_000;
const LIMIT: u64 = 128 * 1024;

fn overflow_name() -> String {
    format!("trace-overflow-{}.arrow", "ab".repeat(16))
}

/// The shared assertions: nothing is dropped, the flag is raised, and the chunks stayed in
/// memory rather than being thrown away (h: the writer keeps chunks beyond the limit).
fn assert_survived_a_full_disk(writer: &std::sync::Arc<TraceWriter>) {
    for seq in 0..RECORDS {
        writer.record(record(seq, (seq % 2) as u16));
    }
    writer.flush().expect("a flush still succeeds");
    let held = writer.memory_bytes();
    let view = writer
        .finish()
        .expect("an overflow failure is not a finish failure");
    assert_eq!(view.len(), RECORDS, "records are counted, never dropped");
    assert!(
        view.overflow_failed(),
        "overflow_failed must be set when a chunk cannot reach the overflow file"
    );
    assert!(
        held > LIMIT,
        "the chunks stayed in memory ({held} bytes) rather than being dropped"
    );
    assert_eq!(view.records().len() as u64, RECORDS, "and are readable");

    // And the report says so, so a run whose trace outgrew memory on a full disk is visible.
    let r = moruna_trace::RunReport::compute(
        &view,
        &common::limits(),
        &common::meta(moruna_trace::ExitReason::Completed),
    );
    assert!(r.overflow_failed);
    assert!(format!("{r}").contains("overflow_failed true"));
}

/// Simulation one: a directory stands where the overflow file would go, so creating the
/// file fails exactly as it would on a filesystem with no room for it.
#[test]
fn tr_t9_disk_full_by_an_unopenable_path() {
    let dir = TempDir::new("t9-path");
    std::fs::create_dir_all(dir.path().join(overflow_name())).expect("the blocking directory");
    let mut cfg = config(dir.path());
    cfg.memory_limit = LIMIT;
    let writer = TraceWriter::start(cfg).expect("start");
    assert_survived_a_full_disk(&writer);
}

/// Simulation two: the staging directory is not a directory, so the overflow file cannot be
/// created there at all. This reaches the same boundary as simulation one through a
/// different errno (`ENOTDIR` rather than `EISDIR`), and neither can be bypassed by a
/// process running as root, which the container gate does.
#[test]
fn tr_t9_disk_full_by_a_staging_path_that_is_not_a_directory() {
    let dir = TempDir::new("t9-notdir");
    let staging = dir.path().join("staging");
    std::fs::write(&staging, b"this is a file, not a directory").expect("the blocking file");

    let mut cfg = config(&staging);
    cfg.memory_limit = LIMIT;
    let writer = TraceWriter::start(cfg).expect("start");
    assert_survived_a_full_disk(&writer);
}

/// h, failures: a final file that cannot be created is refused at start, with the path in
/// the error, rather than discovered when the run is over.
#[test]
fn tr_t9_a_final_file_that_cannot_be_created_is_refused_at_start() {
    let dir = TempDir::new("t9-final");
    let blocked = dir.path().join("trace.arrow");
    std::fs::create_dir_all(&blocked).expect("the blocking directory");
    let mut cfg = config(dir.path());
    cfg.path = Some(blocked.clone());
    let err = TraceWriter::start(cfg).expect_err("start must refuse");
    assert!(
        format!("{err}").contains("trace.arrow"),
        "the error names the path: {err}"
    );

    // A path whose directory does not exist yet is created rather than refused.
    let mut cfg = config(dir.path());
    cfg.path = Some(dir.path().join("nested/deeper/trace.arrow"));
    let writer = TraceWriter::start(cfg).expect("start creates the directory");
    writer.record(record(0, 1));
    let view = writer.finish().expect("finish");
    assert_eq!(view.len(), 1);
    assert!(dir.path().join("nested/deeper/trace.arrow").exists());
}

/// h, failures: writing the view out to a path that cannot be created is an `Io` error
/// naming the path, never a panic.
#[test]
fn tr_t9_to_ipc_file_reports_a_path_it_cannot_write() {
    let dir = TempDir::new("t9-ipc");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    writer.record(record(0, 1));
    let view = writer.finish().expect("finish");

    let blocked = dir.path().join("out.arrow");
    std::fs::create_dir_all(&blocked).expect("the blocking directory");
    let err = view
        .to_ipc_file(&blocked)
        .expect_err("a directory is not a file");
    assert!(format!("{err}").contains("out.arrow"), "{err}");

    // A parent that is a file, not a directory, is reported the same way.
    std::fs::write(dir.path().join("afile"), b"x").expect("a file in the way");
    let err = view
        .to_ipc_file(&dir.path().join("afile/out.arrow"))
        .expect_err("a file is not a directory");
    assert!(format!("{err}").contains("afile"), "{err}");
}
