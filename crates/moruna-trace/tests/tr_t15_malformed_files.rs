//! TR-T15 malformed_files_never_panic. The view reads back two files Moruna wrote, the overflow
//! stream and the final file, and either may be corrupt on disk by the time it does. A final
//! file and an overflow file whose schema names a type id no Arrow type has end what the view
//! returns, without a panic; every corruption and truncation of an Arrow IPC file and stream
//! of every type, put where the view reads them, is read as far as it is valid, never a
//! panic. Proves TR-I7 (contracts CT-I13).

mod common;

use std::path::Path;

use common::{TempDir, config, lean_record};
use moruna_kernel::TraceSink;
use moruna_kernel::arrow::ipc::MetadataVersion;
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_testkit::ipc::{
    batches, corruptions, panics, small, stream, streams, tally, unknown_type,
};
use moruna_trace::{TraceView, TraceWriter};

/// Where the stream inside an Arrow IPC file starts: after the magic and its zero padding.
fn stream_start(file: &[u8]) -> usize {
    file.iter()
        .position(|&b| b == 0xFF)
        .expect("a continuation marker")
}

/// A view whose overflow file holds chunks, and the overflow file's path.
fn spilled(dir: &Path) -> (TraceView, std::path::PathBuf) {
    let mut cfg = config(dir);
    cfg.memory_limit = 1;
    let writer = TraceWriter::start(cfg).expect("start");
    for seq in 0..20_000 {
        writer.record(lean_record(seq, 1));
    }
    writer.flush().expect("flush");
    let view = writer.snapshot();
    let overflow = std::fs::read_dir(dir)
        .expect("the staging dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().contains("trace-overflow-"))
        .expect("an overflow file");
    (view, overflow)
}

/// A finished view over a final file, and the final file's path.
fn finished(dir: &Path) -> (TraceView, std::path::PathBuf) {
    let path = dir.join("trace.arrow");
    let mut cfg = config(dir);
    cfg.path = Some(path.clone());
    let writer = TraceWriter::start(cfg).expect("start");
    for seq in 0..100 {
        writer.record(lean_record(seq, 1));
    }
    (writer.finish().expect("finish"), path)
}

/// An Arrow IPC file of `small()` whose schema, in the stream and in the footer, names type 229.
fn unknown_type_file() -> Vec<u8> {
    let mut file = Vec::new();
    let batch = small();
    let mut writer = FileWriter::try_new(&mut file, &batch.schema()).expect("a file writer");
    writer.write(&batch).expect("a batch");
    writer.finish().expect("the footer");
    drop(writer);
    // The stream starts after the magic and its padding, at the first continuation marker.
    let start = stream_start(&file);
    let mut out = file[..start].to_vec();
    out.extend(unknown_type(&file[start..]));
    // The footer's copy of the schema: the last byte that, set to 229, makes the footer's first
    // field type 229.
    let footer_len = u32::from_le_bytes([
        out[out.len() - 10],
        out[out.len() - 9],
        out[out.len() - 8],
        out[out.len() - 7],
    ]) as usize;
    let footer = out.len() - 10 - footer_len;
    for at in footer..out.len() - 10 {
        let mut bad = out.clone();
        bad[at] = 229;
        let hit = moruna_kernel::arrow::ipc::root_as_footer(&bad[footer..out.len() - 10])
            .ok()
            .and_then(|f| f.schema())
            .and_then(|s| s.fields())
            .is_some_and(|f| !f.is_empty() && f.get(0).type_type().0 == 229);
        if hit {
            return bad;
        }
    }
    panic!("the footer has no type id byte")
}

#[test]
fn tr_t15_unknown_type_ends_the_read() {
    let dir = TempDir::new("t15-final");
    let (view, path) = finished(dir.path());
    std::fs::write(&path, unknown_type_file()).expect("the crafted final file");
    assert_eq!(view.batches().count(), 0);

    let dir = TempDir::new("t15-overflow");
    let (view, overflow) = spilled(dir.path());
    std::fs::write(
        &overflow,
        unknown_type(&stream(&[small()], MetadataVersion::V5)),
    )
    .expect("the crafted overflow file");
    let memory = view.batches().count();
    assert!(memory > 0, "the in-memory chunks are still returned");
}

/// `batches` as an Arrow IPC file, as the writer's final file is.
fn file_of(batches: &[RecordBatch]) -> Vec<u8> {
    let mut file = Vec::new();
    let mut writer = FileWriter::try_new(&mut file, &batches[0].schema()).expect("a file writer");
    for batch in batches {
        writer.write(batch).expect("a batch");
    }
    writer.finish().expect("the footer");
    drop(writer);
    file
}

#[test]
fn tr_t15_malformed_files_never_panic() {
    let mut failed = Vec::new();

    // The final file: corruptions of the stream inside it, and of the whole file.
    let dir = TempDir::new("t15-final-fuzz");
    let (view, path) = finished(dir.path());
    let read = |bytes: Vec<u8>| -> Result<usize, ()> {
        std::fs::write(&path, bytes).map_err(drop)?;
        Ok(view.batches().count())
    };
    for (label, batches, _) in batches() {
        // A file holds one dictionary per field, so the first batch of each.
        let file = file_of(&batches[..1]);
        let start = stream_start(&file);
        let seed = 0x7415_0000 ^ file.len() as u64;
        let inside = corruptions(&file[start..], seed, 200).map(|(case, bytes)| {
            let mut whole = file[..start].to_vec();
            whole.extend(bytes);
            (format!("final {label}, stream {case}"), whole)
        });
        failed.extend(panics(inside, read));
        let whole = corruptions(&file, seed ^ 1, 200)
            .map(|(case, bytes)| (format!("final {label}, file {case}"), bytes));
        failed.extend(panics(whole, read));
    }

    // The overflow file: corruptions of each stream, read for as many chunks as the view
    // believes the file holds.
    let dir = TempDir::new("t15-overflow-fuzz");
    let (view, overflow) = spilled(dir.path());
    let read = |bytes: Vec<u8>| -> Result<usize, ()> {
        std::fs::write(&overflow, bytes).map_err(drop)?;
        Ok(view.batches().count())
    };
    for (label, valid) in streams() {
        let cases = corruptions(&valid, 0x7415_1000 ^ valid.len() as u64, 200)
            .map(|(case, bytes)| (format!("overflow {label}, {case}"), bytes));
        failed.extend(panics(cases, read));
    }

    assert!(failed.is_empty(), "{}", tally(&failed));
}
