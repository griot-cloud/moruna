//! The two binaries as the harness runs them: gate-baseline's RESULT line, gate-canon's digest and
//! comparison, and their refusals.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::RecordBatch;
use arrow::ipc::reader::FileReader;
use parquet::arrow::ArrowWriter;
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gate-bins-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn parquet_of(arrow: &Path, out: &Path) {
    let reader = FileReader::try_new(File::open(arrow).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    let mut w = ArrowWriter::try_new(File::create(out).unwrap(), schema, None).unwrap();
    for b in &batches {
        w.write(b).unwrap();
    }
    w.close().unwrap();
}

fn run(bin: &str, args: &[&str]) -> (i32, String) {
    let o = Command::new(bin).args(args).output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    )
}

#[test]
fn baseline_then_canon() {
    let base = env!("CARGO_BIN_EXE_gate-baseline");
    let canon = env!("CARGO_BIN_EXE_gate-canon");
    let d = scratch("ok");
    let input = d.join("in.parquet");
    parquet_of(&fixtures().join("inputs/nyc/main.arrow"), &input);
    let list = d.join("files");
    std::fs::write(&list, input.to_str().unwrap()).unwrap();
    let out = d.join("out");
    let (code, stdout) = run(
        base,
        &[
            "--job",
            "score",
            "--files",
            list.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--threads",
            "2",
        ],
    );
    assert_eq!(code, 0, "{stdout}");
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("RESULT "))
        .unwrap();
    let r: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        (r["rows_in"].as_u64(), r["function"].as_str()),
        (Some(4000), Some("rust"))
    );
    assert!(r["split"]["function_s"].as_f64().unwrap() > 0.0);
    assert_eq!(r["output"]["zstd_level"].as_u64(), Some(1));
    // The digest, and a comparison of the output with itself.
    let dump = d.join("a.dump");
    let (code, stdout) = run(
        canon,
        &[
            "digest",
            "score",
            "--dump",
            dump.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0);
    let digest: Value = serde_json::from_str(stdout.trim()).unwrap();
    let m: Value =
        serde_json::from_reader(File::open(fixtures().join("manifest.json")).unwrap()).unwrap();
    assert_eq!(digest["hash_all"], m["jobs"]["score"]["main"]["hash_all"]);
    assert_eq!(digest["files"].as_u64(), Some(1));
    let (code, stdout) = run(
        canon,
        &[
            "compare",
            "score",
            dump.to_str().unwrap(),
            dump.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("\"ok\": true"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn refusals() {
    let base = env!("CARGO_BIN_EXE_gate-baseline");
    let canon = env!("CARGO_BIN_EXE_gate-canon");
    let d = scratch("refused");
    assert_eq!(run(base, &[]).0, 2);
    assert_eq!(
        run(base, &["--job", "nope", "--files", "x", "--out", "y"]).0,
        2
    );
    assert_eq!(
        run(
            base,
            &[
                "--job",
                "score",
                "--files",
                d.join("missing").to_str().unwrap(),
                "--out",
                "y"
            ]
        )
        .0,
        2
    );
    // A batch the function refuses: the run fails with the error in its RESULT.
    let input = d.join("in.parquet");
    parquet_of(&fixtures().join("inputs/nyc/null_times.arrow"), &input);
    let list = d.join("files");
    std::fs::write(&list, input.to_str().unwrap()).unwrap();
    let (code, stdout) = run(
        base,
        &[
            "--job",
            "audit",
            "--files",
            list.to_str().unwrap(),
            "--out",
            d.join("o").to_str().unwrap(),
        ],
    );
    assert_eq!(code, 1);
    assert!(stdout.contains("\"error\""));
    assert_eq!(run(canon, &[]).0, 2);
    assert_eq!(run(canon, &["digest", "nope", d.to_str().unwrap()]).0, 2);
    assert_eq!(run(canon, &["compare", "score", "a", "b"]).0, 2);
    let _ = std::fs::remove_dir_all(&d);
}
