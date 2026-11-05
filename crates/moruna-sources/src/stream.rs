//! A source that is one Arrow IPC stream (SDD 19 f.6): what an image source's process writes on
//! its standard output, read a batch at a time as it comes. The stream is the process's, read
//! once and never again, so the source is a streaming one and is not repeatable (a resumed run
//! starts it again from its beginning). When the stream ends, the source asks its `end` whether
//! the writer ended well (an image's process exited with status 0); a failure there fails the
//! read, in the writer's words.

use std::io::{BufReader, Read};
use std::sync::{Arc, Mutex, MutexGuard};

use arrow::ipc::reader::StreamReader;
use moruna_kernel::{
    Allocator, BoxFuture, MorunaError, Payload, Result, RowRange, Source, SourceSchema, Split, Tier,
};

use crate::stats::Counters;

/// How the writer of a stream ended: `Ok` when it ended well, else why, in words. Given what
/// went wrong reading its stream, when anything did.
pub type StreamEnd = Box<dyn FnOnce(Option<String>) -> std::result::Result<(), String> + Send>;

/// The split every batch of a stream is: estimated, never sub splittable.
const STREAM_SPLIT: moruna_kernel::SplitId = 0;

struct State {
    reader: Option<StreamReader<BufReader<Box<dyn Read + Send>>>>,
    end: Option<StreamEnd>,
    ahead: Option<arrow::array::RecordBatch>,
    planned: Vec<Split>,
    exhausted: bool,
}

/// One Arrow IPC stream as a source.
pub struct ArrowStreamSource {
    name: String,
    schema: SourceSchema,
    state: Mutex<State>,
    counters: Arc<Counters>,
}

fn failed(name: &str, why: String) -> MorunaError {
    MorunaError::Source {
        split: STREAM_SPLIT,
        msg: format!("{name}: {why}"),
    }
}

impl ArrowStreamSource {
    /// The source of the stream `stream` holds, named `name` in its errors, whose writer is
    /// judged by `end` when the stream ends. Its schema is read now, so this waits for the
    /// writer's first message; a writer that ends or fails before it is refused, in the words
    /// of `end` and of the stream.
    pub fn new(
        name: &str,
        stream: Box<dyn Read + Send>,
        end: StreamEnd,
    ) -> Result<ArrowStreamSource> {
        let reader = match StreamReader::try_new(BufReader::with_capacity(1 << 20, stream), None) {
            Ok(reader) => reader,
            Err(e) => {
                let why = format!("its output is not an Arrow IPC stream: {e}");
                return Err(failed(
                    name,
                    match end(Some(why.clone())) {
                        Ok(()) => why,
                        Err(ended) => ended,
                    },
                ));
            }
        };
        let schema = SourceSchema::Table(reader.schema());
        Ok(ArrowStreamSource {
            name: name.to_string(),
            schema,
            state: Mutex::new(State {
                reader: Some(reader),
                end: Some(end),
                ahead: None,
                planned: Vec::new(),
                exhausted: false,
            }),
            counters: Arc::new(Counters::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next batch, or `None` once the stream ended and its writer ended well.
    fn pull(&self, state: &mut State) -> Result<Option<arrow::array::RecordBatch>> {
        if let Some(batch) = state.ahead.take() {
            return Ok(Some(batch));
        }
        if state.exhausted {
            return Ok(None);
        }
        let next = state.reader.as_mut().and_then(Iterator::next);
        let fault = match next {
            Some(Ok(batch)) => return Ok(Some(batch)),
            Some(Err(e)) => Some(format!("its output is not an Arrow IPC stream: {e}")),
            None => None,
        };
        state.exhausted = true;
        state.reader = None;
        let end = state.end.take();
        let ended = end.map_or(Ok(()), |end| end(fault.clone()));
        match (fault, ended) {
            (_, Err(why)) => Err(failed(&self.name, why)),
            (Some(why), Ok(())) => Err(failed(&self.name, why)),
            (None, Ok(())) => Ok(None),
        }
    }

    fn land(
        &self,
        batch: &arrow::array::RecordBatch,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        Counters::add(&self.counters.reads, 1);
        let (batch, copied) = crate::parquet::decode_copy::copy_batch(batch, alloc, tier)?;
        Counters::add(&self.counters.decode_bytes, copied);
        alloc.note_payload_copy(copied);
        Payload::table(batch)
    }
}

fn split_of(batch: &arrow::array::RecordBatch) -> Split {
    Split {
        id: STREAM_SPLIT,
        rows: batch.num_rows() as u64,
        uncompressed_bytes: batch.get_array_memory_size() as u64,
        estimated: true,
        column_bytes: batch
            .columns()
            .iter()
            .map(|c| c.get_array_memory_size() as u64)
            .collect(),
        null_counts: batch
            .columns()
            .iter()
            .map(|c| Some(c.null_count() as u64))
            .collect(),
        sub_splittable: false,
    }
}

impl Source for ArrowStreamSource {
    fn schema(&self) -> SourceSchema {
        self.schema.clone()
    }

    /// The first batch's size, as the estimate a streaming source's plan is; empty when the
    /// stream holds none.
    fn plan(&self) -> Result<Vec<Split>> {
        let mut state = self.lock();
        if !state.planned.is_empty() || state.exhausted {
            return Ok(state.planned.clone());
        }
        let planned = match self.pull(&mut state)? {
            Some(batch) => {
                let split = split_of(&batch);
                state.ahead = Some(batch);
                vec![split]
            }
            None => Vec::new(),
        };
        state.planned = planned.clone();
        Ok(planned)
    }

    fn read(
        &self,
        _split: &Split,
        _rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'_, Result<Payload>> {
        let outcome = (|| {
            let mut state = self.lock();
            match self.pull(&mut state)? {
                Some(batch) => self.land(&batch, alloc, tier),
                None => Err(failed(&self.name, "its stream has no more batches".into())),
            }
        })();
        Box::pin(async move { outcome })
    }

    fn streaming(&self) -> bool {
        true
    }

    fn read_next<'a>(
        &'a self,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Option<Payload>>> {
        Box::pin(async move {
            let mut state = self.lock();
            match self.pull(&mut state)? {
                Some(batch) => self.land(&batch, alloc, tier).map(Some),
                None => Ok(None),
            }
        })
    }

    fn repeatable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;

    fn stream(batches: usize) -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let mut out = Vec::new();
        let mut writer = StreamWriter::try_new(&mut out, &schema).unwrap();
        for i in 0..batches {
            let ids = Int64Array::from(vec![i as i64; 3]);
            writer
                .write(&RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(ids)]).unwrap())
                .unwrap();
        }
        writer.finish().unwrap();
        drop(writer);
        out
    }

    fn ok() -> StreamEnd {
        Box::new(|_| Ok(()))
    }

    #[test]
    fn a_stream_is_read_a_batch_at_a_time_then_its_writer_is_judged() {
        let source =
            ArrowStreamSource::new("rows", Box::new(std::io::Cursor::new(stream(3))), ok())
                .expect("source");
        assert!(source.streaming() && !source.repeatable());
        let plan = source.plan().unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].rows, 3);
        let mut state = source.lock();
        let mut rows = 0;
        while let Some(batch) = source.pull(&mut state).unwrap() {
            rows += batch.num_rows();
        }
        assert_eq!(rows, 9);
        drop(state);

        let ended: StreamEnd = Box::new(|fault| {
            assert!(fault.is_none());
            Err("image `rows` exited with status 1".into())
        });
        let source =
            ArrowStreamSource::new("rows", Box::new(std::io::Cursor::new(stream(1))), ended)
                .unwrap();
        let mut state = source.lock();
        assert!(source.pull(&mut state).unwrap().is_some());
        let e = source.pull(&mut state).unwrap_err().to_string();
        assert!(e.contains("exited with status 1"), "{e}");
    }

    #[test]
    fn a_writer_that_writes_no_stream_is_refused_in_its_words() {
        let end: StreamEnd = Box::new(|fault| {
            assert!(fault.unwrap().contains("not an Arrow IPC stream"));
            Err("image `rows` exited with status 2: no such entry".into())
        });
        let e = ArrowStreamSource::new(
            "rows",
            Box::new(std::io::Cursor::new(b"hello".to_vec())),
            end,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(e.contains("no such entry"), "{e}");
    }
}
