//! The gate's split (`arrow-core-0.7.md` §9): the run's `io` carries each source's reads apart
//! from its decodes and each sink's encodes apart from its writes, from the run's meta into the
//! report, its JSON and its text; a half done in the same call is absent from the JSON and said
//! to be one call in the text.

mod common;

use common::{TempDir, config, limits, meta, record};
use moruna_kernel::TraceSink;
use moruna_trace::{ExitReason, IoReport, RunReport, SinkIo, SourceIo, TraceWriter};

fn report(io: IoReport) -> RunReport {
    let dir = TempDir::new("io-split");
    let writer = TraceWriter::start(config(dir.path())).expect("start");
    writer.record(record(0, 1));
    let view = writer.finish().expect("finish");
    let mut meta = meta(ExitReason::Completed);
    meta.io = io;
    RunReport::compute(&view, &limits(), &meta)
}

#[test]
fn reads_decodes_encodes_and_writes_reach_the_report() {
    let r = report(IoReport {
        sources: vec![
            SourceIo {
                reads: 8,
                read_s: 1.25,
                decode_s: Some(3.5),
            },
            SourceIo {
                reads: 2,
                read_s: 0.5,
                decode_s: None,
            },
        ],
        sinks: vec![
            SinkIo {
                writes: 8,
                encode_s: Some(2.0),
                write_s: 0.75,
            },
            SinkIo {
                writes: 1,
                encode_s: None,
                write_s: 0.125,
            },
        ],
    });
    assert_eq!(r.io.sources.len(), 2);
    let text = r.to_string();
    assert!(
        text.contains("source 0: 8 reads, read 1.25 s, decode 3.50 s"),
        "{text}"
    );
    assert!(
        text.contains("source 1: 2 reads, read and decode 0.500 s in one call"),
        "{text}"
    );
    assert!(
        text.contains("sink 0: 8 writes, encode 2.00 s, write 0.750 s"),
        "{text}"
    );
    assert!(
        text.contains("sink 1: 1 writes, encode and write 0.125 s in one call"),
        "{text}"
    );
    let json: serde_json::Value = serde_json::from_str(&r.to_json()).expect("json");
    assert_eq!(json["io"]["sources"][0]["decode_s"], 3.5);
    assert!(json["io"]["sources"][1].get("decode_s").is_none());
    assert_eq!(json["io"]["sinks"][0]["encode_s"], 2.0);
    assert!(json["io"]["sinks"][1].get("encode_s").is_none());
    assert_eq!(json["io"]["sinks"][1]["write_s"], 0.125);
}

#[test]
fn a_run_without_a_scheduler_has_no_split() {
    let r = report(IoReport::default());
    assert!(r.io.sources.is_empty() && r.io.sinks.is_empty());
    let text = r.to_string();
    assert!(!text.contains("source 0:"), "{text}");
    let json: serde_json::Value = serde_json::from_str(&r.to_json()).expect("json");
    assert!(
        json.get("io").is_none(),
        "an empty split leaves the JSON as it was"
    );
}
