//! gate-canon over Parquet output files: every file read in parallel, each a [`Canon`] of its
//! own, merged (the multiset hash adds, so the order does not matter), and the tolerant rows
//! kept in a dump file for a later comparison with another engine's.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use moruna_gate_functions::canon::{Canon, Digest2, TolerantRow};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rayon::prelude::*;

const MAGIC: &[u8] = b"GCANON1\n";

/// The canonical form of the Parquet files under `paths`, with their file and row group counts.
pub fn digest(
    workload: &str,
    paths: &[PathBuf],
    keep_rows: bool,
) -> Result<(Digest2, usize, usize), String> {
    let mut files = Vec::new();
    for p in paths {
        files.extend(crate::parquet_files(p)?);
    }
    let parts: Vec<(Canon, usize)> = files
        .par_iter()
        .map(|f| {
            let b = ParquetRecordBatchReaderBuilder::try_new(
                File::open(f).map_err(|e| format!("{}: {e}", f.display()))?,
            )
            .map_err(|e| format!("{}: {e}", f.display()))?;
            let groups = b.metadata().num_row_groups();
            let mut c = Canon::new(workload, b.schema(), keep_rows).map_err(|e| e.to_string())?;
            for batch in b
                .with_batch_size(65_536)
                .build()
                .map_err(|e| e.to_string())?
            {
                c.add(&batch.map_err(|e| e.to_string())?)
                    .map_err(|e| format!("{}: {e}", f.display()))?;
            }
            Ok((c, groups))
        })
        .collect::<Result<_, String>>()?;
    let mut groups = 0;
    let mut all: Option<Canon> = None;
    for (c, g) in parts {
        groups += g;
        match all.as_mut() {
            None => all = Some(c),
            Some(a) => a.merge(c).map_err(|e| e.to_string())?,
        }
    }
    let all = all.ok_or_else(|| "no Parquet files".to_string())?;
    Ok((
        all.finish().map_err(|e| e.to_string())?,
        files.len(),
        groups,
    ))
}

/// Writes `d` and its tolerant rows: the magic, the digest's JSON on one line, then each row's
/// exact-column hash and its values (a null byte, then the double's bits).
pub fn write_dump(path: &Path, d: &Digest2) -> Result<(), String> {
    let mut w = BufWriter::new(File::create(path).map_err(|e| e.to_string())?);
    let io = |e: std::io::Error| e.to_string();
    w.write_all(MAGIC).map_err(io)?;
    w.write_all(d.json().as_bytes()).map_err(io)?;
    w.write_all(b"\n").map_err(io)?;
    w.write_all(&(d.tolerant_rows.len() as u64).to_le_bytes())
        .map_err(io)?;
    w.write_all(&(d.tolerant.len() as u32).to_le_bytes())
        .map_err(io)?;
    for (h, vals) in &d.tolerant_rows {
        w.write_all(&h.to_le_bytes()).map_err(io)?;
        for v in vals {
            w.write_all(&[v.is_some() as u8]).map_err(io)?;
            w.write_all(&v.unwrap_or(0.0).to_bits().to_le_bytes())
                .map_err(io)?;
        }
    }
    w.flush().map_err(io)
}

fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\": "))? + key.len() + 4;
    let rest = &json[at..];
    let end = rest.find([',', '}'])?;
    Some(rest[..end].trim().trim_matches('"'))
}

fn list(json: &str, key: &str) -> Vec<String> {
    let Some(at) = json.find(&format!("\"{key}\": [")) else {
        return Vec::new();
    };
    let rest = &json[at + key.len() + 5..];
    let end = rest.find(']').unwrap_or(0);
    rest[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Reads a dump [`write_dump`] wrote.
pub fn read_dump(path: &Path) -> Result<Digest2, String> {
    let mut r = BufReader::new(File::open(path).map_err(|e| format!("{}: {e}", path.display()))?);
    let io = |e: std::io::Error| e.to_string();
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(io)?;
    if magic != MAGIC {
        return Err(format!("{} is not a gate-canon dump", path.display()));
    }
    let mut line = String::new();
    r.read_line(&mut line).map_err(io)?;
    let bad = || format!("{}: a damaged digest line", path.display());
    let rows: u64 = field(&line, "rows")
        .and_then(|v| v.parse().ok())
        .ok_or_else(bad)?;
    let mut u64b = [0u8; 8];
    let mut u32b = [0u8; 4];
    r.read_exact(&mut u64b).map_err(io)?;
    r.read_exact(&mut u32b).map_err(io)?;
    let (n, cols) = (u64::from_le_bytes(u64b), u32::from_le_bytes(u32b) as usize);
    let mut tolerant_rows: Vec<TolerantRow> = Vec::with_capacity(n.min(1 << 24) as usize);
    let mut h = [0u8; 16];
    for _ in 0..n {
        r.read_exact(&mut h).map_err(io)?;
        let mut vals = Vec::with_capacity(cols);
        for _ in 0..cols {
            let mut flag = [0u8; 1];
            r.read_exact(&mut flag).map_err(io)?;
            r.read_exact(&mut u64b).map_err(io)?;
            vals.push((flag[0] == 1).then(|| f64::from_bits(u64::from_le_bytes(u64b))));
        }
        tolerant_rows.push((u128::from_le_bytes(h), vals));
    }
    Ok(Digest2 {
        rows,
        hash_exact: field(&line, "hash_exact").ok_or_else(bad)?.to_string(),
        hash_all: field(&line, "hash_all").ok_or_else(bad)?.to_string(),
        exact: list(&line, "exact"),
        tolerant: list(&line, "tolerant"),
        tolerant_rows,
    })
}
