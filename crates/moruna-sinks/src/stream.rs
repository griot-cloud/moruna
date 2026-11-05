//! A sink that is one Arrow IPC stream (SDD 19 f.6): what an image sink's process reads on its
//! standard input. Every batch is written to the stream as it comes; at the end the stream is
//! ended and closed, and the sink's `end` is asked how its reader ended (an image's process
//! exited with status 0) and what it wrote back: nothing, or one JSON object naming the version
//! it committed, `{"snapshot_id": ..., "version": ...}`.

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use arrow::ipc::writer::StreamWriter;
use moruna_kernel::{
    BoxFuture, MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkCommit,
    SinkSummary, SourceSchema, TierPref,
};

/// How a stream's reader ended: what it wrote back when it ended well, else why, in words.
pub type StreamDone = Box<dyn FnOnce() -> std::result::Result<Vec<u8>, String> + Send>;

enum Phase {
    Created(Box<dyn Write + Send>),
    Open(Box<StreamWriter<Box<dyn Write + Send>>>),
    Failed(String),
    Finished,
}

struct Inner {
    phase: Phase,
    done: Option<StreamDone>,
    rows: u64,
    bytes: u64,
}

/// One Arrow IPC stream as a sink.
pub struct ArrowStreamSink {
    name: String,
    inner: Mutex<Inner>,
}

impl ArrowStreamSink {
    /// The sink that writes the stream into `stream`, named `name` in its errors, whose reader
    /// is judged by `done` at the end.
    pub fn new(name: &str, stream: Box<dyn Write + Send>, done: StreamDone) -> ArrowStreamSink {
        ArrowStreamSink {
            name: name.to_string(),
            inner: Mutex::new(Inner {
                phase: Phase::Created(stream),
                done: Some(done),
                rows: 0,
                bytes: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_now(&self, payload: Payload) -> Result<()> {
        let mut inner = self.lock();
        let Payload::Table(batch, _) = payload else {
            return Err(MorunaError::Sink(format!(
                "{} takes tables; the job gives it something else",
                self.name
            )));
        };
        let written = match &mut inner.phase {
            Phase::Open(writer) => writer.write(&batch).map_err(|e| e.to_string()),
            Phase::Failed(why) => return Err(MorunaError::Sink(why.clone())),
            _ => return Err(MorunaError::Sink(format!("{} is not open", self.name))),
        };
        if let Err(e) = written {
            let why = format!("{}: its input could not be written: {e}", self.name);
            inner.phase = Phase::Failed(why.clone());
            return Err(MorunaError::Sink(why));
        }
        inner.rows += batch.num_rows() as u64;
        inner.bytes += batch.get_array_memory_size() as u64;
        Ok(())
    }
}

/// What a reader wrote back at its end as the version it committed: nothing (or only
/// whitespace) for none; else one JSON object with `snapshot_id` (a string or an integer) and
/// optionally `version`, and nothing else.
pub fn commit_of(written: &[u8]) -> std::result::Result<Option<SinkCommit>, String> {
    let text = std::str::from_utf8(written).map_err(|_| "wrote back bytes that are not UTF-8")?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("wrote back what is not one JSON object: {e}"))?;
    let Some(map) = value.as_object() else {
        return Err(format!("wrote back {value}; a commit is a JSON object"));
    };
    if let Some(other) = map.keys().find(|k| *k != "snapshot_id" && *k != "version") {
        return Err(format!(
            "wrote back a commit with `{other}`; a commit names `snapshot_id` and `version`"
        ));
    }
    let snapshot_id = match map.get("snapshot_id") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::Number(n)) if n.is_i64() => n.to_string(),
        None | Some(serde_json::Value::Null) => {
            return Err(
                "wrote back a commit without `snapshot_id`; every commit names its snapshot".into(),
            );
        }
        Some(other) => {
            return Err(format!(
                "wrote back the `snapshot_id` {other}, which is neither a non-empty string nor \
                 an int"
            ));
        }
    };
    let version = match map.get("version") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };
    Ok(Some(SinkCommit {
        snapshot_id,
        version,
    }))
}

impl Sink for ArrowStreamSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let mut inner = self.lock();
        let SourceSchema::Table(schema) = schema else {
            return Err(MorunaError::Sink(format!(
                "{} takes tables; the job gives it tensors",
                self.name
            )));
        };
        let phase = std::mem::replace(&mut inner.phase, Phase::Finished);
        let Phase::Created(stream) = phase else {
            inner.phase = phase;
            return Err(MorunaError::Sink(format!("{} was opened twice", self.name)));
        };
        inner.phase = match StreamWriter::try_new(stream, schema) {
            Ok(writer) => Phase::Open(Box::new(writer)),
            Err(e) => {
                let why = format!("{}: its input could not be started: {e}", self.name);
                inner.phase = Phase::Failed(why.clone());
                return Err(MorunaError::Sink(why));
            }
        };
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn write(&self, _seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        let outcome = self.write_now(payload);
        Box::pin(async move { outcome })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let mut inner = self.lock();
        let phase = std::mem::replace(&mut inner.phase, Phase::Finished);
        let ended = match phase {
            Phase::Open(mut writer) => writer
                .finish()
                .map_err(|e| format!("{}: its input could not be ended: {e}", self.name))
                .and_then(|()| {
                    // Closing the stream is how its reader learns it has everything.
                    writer
                        .into_inner()
                        .map(drop)
                        .map_err(|e| format!("{}: its input could not be closed: {e}", self.name))
                }),
            Phase::Created(stream) => {
                drop(stream);
                Ok(())
            }
            Phase::Failed(why) => Err(why),
            Phase::Finished => return Err(MorunaError::Sink("finish ran twice".into())),
        };
        let done = inner.done.take();
        let written = done.map_or(Ok(Vec::new()), |done| done());
        let written = match (ended, written) {
            (_, Err(why)) | (Err(why), Ok(_)) => return Err(MorunaError::Sink(why)),
            (Ok(()), Ok(written)) => written,
        };
        let commit =
            commit_of(&written).map_err(|why| MorunaError::Sink(format!("{} {why}", self.name)))?;
        Ok(SinkSummary {
            rows: inner.rows,
            bytes: inner.bytes,
            files: Vec::new(),
            write: None,
            commit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn commits_are_read_from_what_the_reader_wrote_back() {
        assert_eq!(commit_of(b"").unwrap(), None);
        assert_eq!(commit_of(b" \n").unwrap(), None);
        let c = commit_of(br#"{"snapshot_id": 42, "version": "v1"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.snapshot_id.as_str(), c.version.as_deref()),
            ("42", Some("v1"))
        );
        assert!(
            commit_of(br#"{"version": "v1"}"#)
                .unwrap_err()
                .contains("without `snapshot_id`")
        );
        assert!(
            commit_of(br#"{"snapshot_id": "a", "rows": 1}"#)
                .unwrap_err()
                .contains("`rows`")
        );
        assert!(commit_of(b"[1]").is_err());
        assert!(commit_of(b"done").is_err());
    }

    #[test]
    fn every_batch_reaches_the_stream_and_the_reader_is_judged() {
        let shared = Shared::default();
        let done: StreamDone = Box::new(|| Ok(br#"{"snapshot_id": "s1"}"#.to_vec()));
        let mut sink = ArrowStreamSink::new("out", Box::new(shared.clone()), done);
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        sink.open(&SourceSchema::Table(Arc::clone(&schema)))
            .unwrap();
        for _ in 0..2 {
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(Int64Array::from(vec![1, 2, 3]))],
            )
            .unwrap();
            let mut write = sink.write(0, Payload::table(batch).unwrap());
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            match write.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(done) => done.unwrap(),
                std::task::Poll::Pending => panic!("a stream write is ready at once"),
            }
        }
        let summary = sink.finish().unwrap();
        assert_eq!(summary.rows, 6);
        assert_eq!(summary.commit.unwrap().snapshot_id, "s1");
        let bytes = shared.0.lock().unwrap().clone();
        let reader =
            arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        assert_eq!(reader.map(|b| b.unwrap().num_rows()).sum::<usize>(), 6);

        let failed: StreamDone = Box::new(|| Err("image `out` exited with status 1".into()));
        let mut sink = ArrowStreamSink::new("out", Box::new(Shared::default()), failed);
        sink.open(&SourceSchema::Table(schema)).unwrap();
        let e = sink.finish().unwrap_err().to_string();
        assert!(e.contains("exited with status 1"), "{e}");
    }
}
