//! The boot fixture writers, run as `kernel/tools/build.sh` runs them: what they write, and
//! how they refuse a wrong invocation or an unwritable target.

use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    // Each test passes its own name, so pid + name is unique across parallel tests.
    let dir = std::env::temp_dir().join(format!("m-tk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A length-prefixed fixture: the u64 length must match the bytes that follow.
fn framed(path: &std::path::Path) -> usize {
    let bytes = std::fs::read(path).unwrap();
    let len = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len() - 8, "{}", path.display());
    len
}

#[test]
fn plane_fixture_writes_every_framed_fixture() {
    let dir = scratch("plane");
    let out = Command::new(env!("CARGO_BIN_EXE_plane-fixture"))
        .arg(dir.join("plane"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for name in ["good", "big", "mib", "echo", "refusals"] {
        assert!(
            framed(&dir.join(format!("plane/{name}.fixture"))) > 0,
            "{name}"
        );
    }
    // The refusal list starts with its case count.
    let refusals = std::fs::read(dir.join("plane/refusals.fixture")).unwrap();
    assert!(u32::from_le_bytes(refusals[8..12].try_into().unwrap()) > 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn plane_fixture_refuses_a_wrong_invocation_and_an_unwritable_target() {
    let usage = Command::new(env!("CARGO_BIN_EXE_plane-fixture"))
        .output()
        .unwrap();
    assert!(!usage.status.success());
    assert!(String::from_utf8_lossy(&usage.stderr).contains("usage: plane-fixture"));
    let dir = scratch("plane-blocked");
    let file = dir.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let blocked = Command::new(env!("CARGO_BIN_EXE_plane-fixture"))
        .arg(file.join("plane"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).starts_with("plane-fixture: "));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn kd2_fixture_writes_the_rows_the_embedder_checks() {
    let dir = scratch("kd2");
    let out = Command::new(env!("CARGO_BIN_EXE_kd2-fixture"))
        .arg(dir.join("kd2"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = std::fs::File::open(dir.join("kd2/input.arrow")).unwrap();
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let mut rows = 0i64;
    for batch in reader {
        let batch = batch.unwrap();
        let x = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        let y = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            assert_eq!(x.value(i), rows);
            assert_eq!(y.value(i), (rows * 7919) % 10007);
            rows += 1;
        }
    }
    assert_eq!(rows, 100_000);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn kd2_fixture_refuses_a_wrong_invocation_and_an_unwritable_target() {
    let usage = Command::new(env!("CARGO_BIN_EXE_kd2-fixture"))
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("usage: kd2-fixture"));
    let dir = scratch("kd2-blocked");
    let file = dir.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let blocked = Command::new(env!("CARGO_BIN_EXE_kd2-fixture"))
        .arg(file.join("kd2"))
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&blocked.stderr).starts_with("kd2-fixture: "));
    std::fs::remove_dir_all(&dir).unwrap();
}
