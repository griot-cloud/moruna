//! The data plane's boot fixtures (SDD 29 k), written by `kernel/tools/build.sh` into the
//! boot's `plane/` directory, each a little-endian `u64` length and then its bytes:
//!
//! - `good.fixture`: a record (contracts e.7) of eight rows, `x: Int64` and `s: Utf8`;
//! - `big.fixture` and `mib.fixture`: the framing (one page) of a record of one `Int64`
//!   column of 8 Mi and of 128 Ki values, 64 MiB and 1 MiB of body, which a task writes
//!   the values of itself;
//! - `echo.fixture`: a record of 1,000 rows, `x: Int64` and `s: Utf8`, for `arrow_echo`;
//! - `refusals.fixture`: the records DP-T12 puts and the words the refusal of each must
//!   hold: garbage, a record put shorter than its buffers reach, a dictionary column, and
//!   every one of KA10's compressed bombs (`moruna-kernel/tests/bombs`) as a record.
//!
//!   plane-fixture OUT

use std::path::Path;
use std::process::ExitCode;

use moruna_testkit::plane::{framing, mixed, record, refusals};

fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut out = (bytes.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(bytes);
    std::fs::write(dir.join(name), out).map_err(|e| format!("writing {name}: {e}"))
}

/// One refusal case: its label, how many bytes the task puts, what the refusal says, and
/// the bytes.
fn case(out: &mut Vec<u8>, label: &str, put: u64, says: &str, bytes: &[u8]) {
    out.extend_from_slice(&(label.len() as u32).to_le_bytes());
    out.extend_from_slice(label.as_bytes());
    out.extend_from_slice(&put.to_le_bytes());
    out.extend_from_slice(&(says.len() as u32).to_le_bytes());
    out.extend_from_slice(says.as_bytes());
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn fixtures(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    write(dir, "good.fixture", &record(&mixed(8)?)?)?;
    write(dir, "big.fixture", &framing(8 << 20)?)?;
    write(dir, "mib.fixture", &framing(128 << 10)?)?;
    write(dir, "echo.fixture", &record(&mixed(1000)?)?)?;
    let all = refusals()?;
    let mut cases = (all.len() as u32).to_le_bytes().to_vec();
    for r in &all {
        case(&mut cases, &r.label, r.put, r.says, &r.bytes);
    }
    write(dir, "refusals.fixture", &cases)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, dir] = args.as_slice() else {
        eprintln!("usage: plane-fixture OUT");
        return ExitCode::FAILURE;
    };
    match fixtures(Path::new(dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("plane-fixture: {why}");
            ExitCode::FAILURE
        }
    }
}
