//! gate-canon: the canonical form and content hash of a job's output (bench/vm/canon.py's
//! spec), for any engine's Parquet files.
//!
//!   gate-canon digest WORKLOAD [--dump FILE] PATH...   prints the digest as JSON
//!   gate-canon compare WORKLOAD A.dump B.dump          prints the comparison; exit 1 if it fails

use std::path::PathBuf;

use moruna_gate_baseline::canonical::{digest, read_dump, write_dump};
use moruna_gate_functions::canon::compare;

fn fail(m: &str) -> ! {
    eprintln!("gate-canon: {m}");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("digest") if args.len() >= 3 => {
            let workload = &args[1];
            let mut dump = None;
            let mut paths = Vec::new();
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                if a == "--dump" {
                    dump = it.next().map(PathBuf::from);
                } else {
                    paths.push(PathBuf::from(a));
                }
            }
            let (d, files, groups) =
                digest(workload, &paths, dump.is_some()).unwrap_or_else(|e| fail(&e));
            if let Some(p) = dump {
                write_dump(&p, &d).unwrap_or_else(|e| fail(&e));
            }
            let j = d.json();
            println!(
                "{}, \"files\": {files}, \"row_groups\": {groups}}}",
                &j[..j.len() - 1]
            );
        }
        Some("compare") if args.len() == 4 => {
            let a = read_dump(args[2].as_ref()).unwrap_or_else(|e| fail(&e));
            let b = read_dump(args[3].as_ref()).unwrap_or_else(|e| fail(&e));
            let c = compare(&args[1], &a, &b).unwrap_or_else(|e| fail(&e.to_string()));
            println!("{c}");
            std::process::exit(if c.ok { 0 } else { 1 });
        }
        _ => fail(
            "usage: gate-canon digest WORKLOAD [--dump FILE] PATH... | compare WORKLOAD A.dump B.dump",
        ),
    }
}
