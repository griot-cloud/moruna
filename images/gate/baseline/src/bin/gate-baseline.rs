//! gate-baseline: one gate job in a plain rayon pool over arrow-rs and parquet (control 2).
//!
//!   gate-baseline --job JOB --files LIST --out DIR [--threads N] [--batch ROWS]
//!
//! LIST is a file of input paths, one per line or space separated, as `runner.py` writes it.
//! Prints one `RESULT {json}` line: wall time, rows, files, the time split and the output
//! settings.

use std::path::PathBuf;
use std::time::Instant;

use moruna_gate_baseline::{Output, run};
use moruna_gate_functions::batch::{by_name, known};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(job), Some(list), Some(out)) = (get("--job"), get("--files"), get("--out")) else {
        eprintln!(
            "usage: gate-baseline --job JOB --files LIST --out DIR [--threads N] [--batch ROWS]"
        );
        std::process::exit(2);
    };
    let threads: usize = get("--threads").and_then(|v| v.parse().ok()).unwrap_or(0);
    let batch: usize = get("--batch").and_then(|v| v.parse().ok()).unwrap_or(0);
    let Some(f) = by_name(&job) else {
        eprintln!("gate-baseline: no job {job:?}; the jobs are {}", known());
        std::process::exit(2);
    };
    let files: Vec<PathBuf> = match std::fs::read_to_string(&list) {
        Ok(s) => s.split_whitespace().map(PathBuf::from).collect(),
        Err(e) => {
            eprintln!("gate-baseline: {list}: {e}");
            std::process::exit(2);
        }
    };
    let output = Output::default();
    let workers = if threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        threads
    };
    let t0 = Instant::now();
    let result = run(&files, f, out.as_ref(), threads, batch, output);
    let wall = t0.elapsed().as_secs_f64();
    match result {
        Ok(o) => {
            let busy = o.split.read_decode_s + o.split.function_s + o.split.encode_write_s;
            println!(
                "RESULT {{\"engine\": \"rust-baseline\", \"workload\": \"{job}\", \"function\": \"rust\", \"wall_s\": {wall:.3}, \"threads\": {workers}, \"batch\": {batch}, \"units\": {}, \"rows_in\": {}, \"rows_out\": {}, \"files_out\": {}, \"split\": {{\"read_decode_s\": {:.3}, \"function_s\": {:.3}, \"encode_write_s\": {:.3}, \"wait_s\": {:.3}}}, \"output\": {}}}",
                o.units,
                o.rows_in,
                o.rows_out,
                o.files_out,
                o.split.read_decode_s,
                o.split.function_s,
                o.split.encode_write_s,
                (wall * workers as f64 - busy).max(0.0),
                output.json()
            );
        }
        Err(e) => {
            println!(
                "RESULT {{\"engine\": \"rust-baseline\", \"workload\": \"{job}\", \"wall_s\": {wall:.3}, \"error\": \"{}\"}}",
                e.replace('\\', "\\\\").replace('"', "'")
            );
            std::process::exit(1);
        }
    }
}
