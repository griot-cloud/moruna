//! `PySink`, a user's `moruna.Sink` subclass: SI-T18 to SI-T22 (08 k, f.10, e.6).
//!
//! Each fake is a Python class written inline and imported under a module name of its own, run in
//! the interpreter `PYO3_PYTHON` names (`MORUNA_PYTHON` in the gate), which must import pyarrow.
//! Built only with the `python` feature.

#![cfg(feature = "python")]
// `MorunaError` is the contract's and large (CT-I10); the helpers return it as the crate does.
#![allow(clippy::result_large_err)]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{arena_payload, arena_tensor, block_on, block_on_all, table_source_schema};
use moruna_kernel::{MorunaError, PayloadKind, Result, Seq, Sink, SourceSchema, TierPref};
use moruna_sinks::{PySink, SinkHandle};
use moruna_testkit::FakeAllocator;
use pyo3::prelude::*;

const FAKES: &str = r#"
import pyarrow as pa


class Collect:
    def __init__(self):
        self.firsts = []
        self.rows = []
        self.finished = 0
        self.schemas = []

    def write(self, batch):
        assert isinstance(batch, pa.RecordBatch)
        if str(batch.schema) not in self.schemas:
            self.schemas.append(str(batch.schema))
        self.firsts.append(batch.column(0)[0].as_py())
        self.rows.extend(batch.column(0).to_pylist())

    def finish(self):
        self.finished += 1


class Durable(Collect):
    """Its state is every value written so far; restore replaces what it holds with it."""

    def checkpoint(self):
        return ",".join(map(str, self.rows)).encode()

    def restore(self, state):
        self.restored = bytes(state)
        self.rows = [int(v) for v in state.decode().split(",")] if state else []


class Fails(Collect):
    def write(self, batch):
        if len(self.rows) >= 20:
            raise OSError(f"disk full at row {len(self.rows)}")
        super().write(batch)


class NoFinish:
    def __init__(self):
        self.rows = []

    def write(self, batch):
        self.rows.extend(batch.column(0).to_pylist())


class NoWrite:
    pass


class WriteNotCallable:
    write = 3


class CheckpointFlips(Collect):
    calls = 0

    def checkpoint(self):
        self.calls += 1
        return b"x" if self.calls == 1 else None


class CheckpointNotBytes(Collect):
    def checkpoint(self):
        return 42


class CheckpointRaises(Collect):
    def checkpoint(self):
        raise RuntimeError("no state to give")


class NoRestore(Collect):
    def checkpoint(self):
        return b"s"


class RestoreRaises(Durable):
    def restore(self, state):
        raise ValueError("corrupt state")


class FinishRaises(Collect):
    def finish(self):
        raise RuntimeError("flush failed")
"#;

/// A new instance of fake `class`.
fn fake(class: &str) -> Py<PyAny> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // One at a time: two threads importing pyarrow at once under the free-threaded interpreter
    // can see the module half initialised.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    Python::initialize();
    Python::attach(|py| {
        let name = format!("si_python_{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let module = pyo3::types::PyModule::from_code(
            py,
            std::ffi::CString::new(FAKES).expect("the fakes").as_c_str(),
            std::ffi::CString::new(format!("{name}.py"))
                .expect("a file name")
                .as_c_str(),
            std::ffi::CString::new(name.clone())
                .expect("a module name")
                .as_c_str(),
        )
        .expect("the fakes import (is pyarrow on PYTHONPATH?)");
        module
            .getattr(class)
            .expect("the class")
            .call0()
            .expect("the fake builds")
            .unbind()
    })
}

/// A sink over a new `class`, and the object, to look at afterwards.
fn sink(class: &str) -> (PySink, Py<PyAny>) {
    let object = fake(class);
    let clone = Python::attach(|py| object.clone_ref(py));
    (PySink::new(object).expect("the sink builds"), clone)
}

/// An int attribute of the user's object.
fn number(object: &Py<PyAny>, name: &str) -> u64 {
    Python::attach(|py| {
        object
            .bind(py)
            .getattr(name)
            .expect("the attribute")
            .extract::<u64>()
            .expect("an int")
    })
}

/// A list-of-ints attribute of the user's object.
fn ints(object: &Py<PyAny>, name: &str) -> Vec<i64> {
    Python::attach(|py| {
        object
            .bind(py)
            .getattr(name)
            .expect("the attribute")
            .extract::<Vec<i64>>()
            .expect("a list of ints")
    })
}

/// The length of a list attribute of the user's object.
fn count(object: &Py<PyAny>, name: &str) -> usize {
    Python::attach(|py| {
        object
            .bind(py)
            .getattr(name)
            .expect("the attribute")
            .len()
            .expect("a list")
    })
}

/// The ten rows of sequence `seq`'s morsel start at `seq * 100`.
fn write(sink: &dyn Sink, alloc: &FakeAllocator, seq: Seq) -> Result<()> {
    block_on(sink.write(seq, arena_payload(alloc, 10, seq as i64 * 100)))
}

fn rows_of(seqs: impl IntoIterator<Item = Seq>) -> Vec<i64> {
    let mut out: Vec<i64> = seqs
        .into_iter()
        .flat_map(|seq| (0..10).map(move |r| seq as i64 * 100 + r))
        .collect();
    out.sort_unstable();
    out
}

/// SI-T18 py_sink_ordered_writes. f.9, f.10, SI-I5, SI-I7: through an ordered `SinkHandle` every
/// batch reaches the user's `write` in sequence order, whatever order the writes arrive in; a
/// plain handle delivers them as they arrive. The summary is the rows and bytes the user's
/// batches measure.
#[test]
fn si_t18_py_sink_ordered_writes() {
    let alloc = FakeAllocator::new();
    let arrival: Vec<Seq> = vec![3, 0, 7, 1, 2, 6, 4, 5, 9, 8];

    let (py_sink, object) = sink("Collect");
    let mut handle = SinkHandle::wrap(Box::new(py_sink), true, 1 << 30);
    assert!(handle.is_ordered());
    handle.open(&table_source_schema()).expect("open");
    let futures: Vec<Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>> = arrival
        .iter()
        .map(|&seq| handle.write(seq, arena_payload(&alloc, 10, seq as i64 * 100)))
        .collect();
    for outcome in block_on_all(futures) {
        outcome.expect("every write lands");
    }
    assert_eq!(handle.committed_seq(), Some(9));
    let summary = handle.finish().expect("finish");
    let firsts = ints(&object, "firsts");
    assert_eq!(
        firsts,
        (0..10).map(|s| s * 100).collect::<Vec<_>>(),
        "in order"
    );
    assert_eq!(number(&object, "finished"), 1);
    assert_eq!(summary.rows, 100);
    // Two int64 columns of ten rows, as `pyarrow.RecordBatch.nbytes` measures them.
    assert_eq!(summary.bytes, 10 * 160);
    assert!(summary.files.is_empty(), "a Python sink names no files");
    assert_eq!(
        count(&object, "schemas"),
        1,
        "every batch had the one schema"
    );

    let (py_sink, object) = sink("Collect");
    let mut plain = SinkHandle::wrap(Box::new(py_sink), false, 1 << 30);
    assert!(
        !plain.is_ordered(),
        "a Python sink does not require order of its own"
    );
    plain.open(&table_source_schema()).expect("open");
    for &seq in &arrival {
        block_on(plain.write(seq, arena_payload(&alloc, 10, seq as i64 * 100))).expect("write");
    }
    plain.finish().expect("finish");
    let firsts = ints(&object, "firsts");
    assert_eq!(
        firsts,
        arrival.iter().map(|&s| s as i64 * 100).collect::<Vec<_>>(),
        "as they arrived"
    );
}

/// SI-T19 py_sink_finish_once. SI-I2, e.1: `finish` reaches the user's object exactly once, a
/// second `finish` and a `write` after it are `Sink` errors, and so are a `write` or `finish`
/// before `open`.
#[test]
fn si_t19_py_sink_finish_once() {
    let alloc = FakeAllocator::new();
    let (mut py_sink, object) = sink("Collect");
    assert!(
        write(&py_sink, &alloc, 0)
            .unwrap_err()
            .to_string()
            .contains("not open")
    );
    assert!(
        py_sink
            .finish()
            .unwrap_err()
            .to_string()
            .contains("not open")
    );
    py_sink.open(&table_source_schema()).expect("open");
    assert!(py_sink.open(&table_source_schema()).is_err(), "open once");
    write(&py_sink, &alloc, 0).expect("write");
    py_sink.finish().expect("finish");
    assert_eq!(number(&object, "finished"), 1);
    assert!(py_sink.finish().unwrap_err().to_string().contains("twice"));
    assert!(
        write(&py_sink, &alloc, 1)
            .unwrap_err()
            .to_string()
            .contains("finished")
    );
    assert_eq!(number(&object, "finished"), 1, "still once");

    // `finish` is optional on the user's class.
    let (mut py_sink, object) = sink("NoFinish");
    py_sink.open(&table_source_schema()).expect("open");
    write(&py_sink, &alloc, 0).expect("write");
    assert_eq!(py_sink.finish().expect("finish").rows, 10);
    assert_eq!(ints(&object, "rows").len(), 10);
}

/// SI-T20 py_sink_checkpoint_resume. e.6, f.10, SI-I8 in the form a Python sink can keep: a sink
/// checkpointed while the scheduler's watermark lags it, killed without `finish`, and resumed in
/// a new object from that checkpoint holds every row exactly once when the scheduler delivers
/// again everything above the watermark.
#[test]
fn si_t20_py_sink_checkpoint_resume() {
    let alloc = FakeAllocator::new();
    let (mut first, _) = sink("Durable");
    let at_start = first.checkpoint().expect("a checkpoint at startup");
    assert!(
        at_start.is_some(),
        "a sink that checkpoints is resumable from the start"
    );
    first.open(&table_source_schema()).expect("open");
    for seq in [0, 1, 2, 3, 4, 5, 7] {
        write(&first, &alloc, seq).expect("write");
    }
    assert_eq!(first.committed_seq(), Some(5), "6 has not arrived");
    // The scheduler read its watermark (3) a moment before asking for the state (SC f.12).
    let watermark = Some(3);
    let state = first.checkpoint().expect("checkpoint").expect("resumable");
    // Killed: more writes land after the checkpoint and are lost with the process.
    write(&first, &alloc, 6).expect("write");
    drop(first);

    let (mut second, object) = sink("Durable");
    second
        .resume(&table_source_schema(), &state, watermark)
        .expect("resume");
    let restored = ints(&object, "rows");
    assert_eq!(
        restored,
        rows_of([0, 1, 2, 3, 4, 5, 7]),
        "restore got the user's bytes"
    );
    assert_eq!(second.committed_seq(), watermark);
    // The scheduler delivers again everything above the watermark, in any order.
    for seq in [5, 4, 6, 9, 7, 8] {
        write(&second, &alloc, seq).expect("write");
    }
    let stats = second.stats();
    assert_eq!(
        stats.replayed, 3,
        "4, 5 and 7 were already in the restored state"
    );
    assert_eq!(second.committed_seq(), Some(9));
    let summary = second.finish().expect("finish");
    let mut rows = ints(&object, "rows");
    rows.sort_unstable();
    assert_eq!(rows, rows_of(0..10), "every row exactly once");
    assert_eq!(summary.rows, 100, "SI-I7 across the resume");
    assert_eq!(
        stats.writes, 10,
        "seven before the checkpoint, three after the resume"
    );

    // A checkpoint that holds less than the watermark would lose rows, and is refused.
    let (mut third, _) = sink("Durable");
    let error = third
        .resume(&table_source_schema(), &state, Some(6))
        .unwrap_err();
    assert!(matches!(error, MorunaError::Resume(_)), "{error}");
    assert!(error.to_string().contains("would be lost"), "{error}");
    // So is state that is not this sink's.
    let (mut fourth, _) = sink("Durable");
    let error = fourth
        .resume(&table_source_schema(), b"{\"version\":1}", Some(0))
        .unwrap_err();
    assert!(
        error.to_string().contains("not a python sink checkpoint"),
        "{error}"
    );
    // A resumed sink is open; resuming or opening it again is refused.
    let (mut fifth, _) = sink("Durable");
    fifth
        .resume(&table_source_schema(), &state, None)
        .expect("resume from nothing committed");
    assert!(fifth.open(&table_source_schema()).is_err());
    assert!(fifth.resume(&table_source_schema(), &state, None).is_err());
}

/// SI-T21 py_sink_write_exception. h, e.1: an exception in the user's `write` fails the write
/// with a `Sink` error carrying the Python message; the sink is `Failed`, later writes are
/// refused, and `finish` returns the error without calling the user's `finish`.
#[test]
fn si_t21_py_sink_write_exception() {
    let alloc = FakeAllocator::new();
    let (mut py_sink, object) = sink("Fails");
    py_sink.open(&table_source_schema()).expect("open");
    write(&py_sink, &alloc, 0).expect("write");
    write(&py_sink, &alloc, 1).expect("write");
    let error = write(&py_sink, &alloc, 2).unwrap_err();
    assert!(matches!(error, MorunaError::Sink(_)), "{error}");
    let text = error.to_string();
    assert!(
        text.contains("Fails.write(): OSError: disk full at row 20"),
        "{text}"
    );
    assert_eq!(
        py_sink.committed_seq(),
        Some(1),
        "the failed write is not committed"
    );
    let later = write(&py_sink, &alloc, 3).unwrap_err();
    assert!(later.to_string().contains("disk full"), "{later}");
    let finished = py_sink.finish().unwrap_err();
    assert!(finished.to_string().contains("disk full"), "{finished}");
    assert_eq!(
        number(&object, "finished"),
        0,
        "finish is not called after a failure"
    );

    let (mut py_sink, _) = sink("FinishRaises");
    py_sink.open(&table_source_schema()).expect("open");
    let error = py_sink.finish().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("FinishRaises.finish(): RuntimeError: flush failed"),
        "{error}"
    );
}

/// SI-T22 py_sink_contract_edges. d.8, SC f.11, f.10: a sink whose `checkpoint` returns `None` is
/// not resumable and says so the way the contract spells it; it still keeps the watermark,
/// `skip` included. Every other refusal names the user's class and what is wrong.
#[test]
fn si_t22_py_sink_contract_edges() {
    let alloc = FakeAllocator::new();
    let (mut py_sink, _) = sink("Collect");
    assert_eq!(py_sink.checkpoint().expect("checkpoint"), None);
    let accepts = py_sink.accepts();
    assert_eq!(accepts.kind, PayloadKind::Table);
    assert_eq!(accepts.tier, TierPref::Host);
    assert!(!py_sink.requires_order());
    py_sink.open(&table_source_schema()).expect("open");
    assert_eq!(py_sink.committed_seq(), None);
    write(&py_sink, &alloc, 0).expect("write");
    py_sink.skip(1);
    py_sink.skip(0);
    write(&py_sink, &alloc, 2).expect("write");
    assert_eq!(py_sink.committed_seq(), Some(2), "skips count as committed");
    assert_eq!(
        py_sink.checkpoint().expect("checkpoint"),
        None,
        "still not resumable"
    );
    let error = write(&py_sink, &alloc, 3);
    assert!(error.is_ok());
    let tensor = arena_tensor(&alloc, 4, 2, 0.0);
    let error = block_on(py_sink.write(4, tensor)).unwrap_err();
    assert!(error.to_string().contains("takes tables"), "{error}");

    for class in ["NoWrite", "WriteNotCallable"] {
        let error = match PySink::new(fake(class)) {
            Ok(_) => panic!("{class} was accepted"),
            Err(e) => e,
        };
        assert!(matches!(error, MorunaError::Plan(_)), "{class}: {error}");
        assert!(
            error.to_string().contains("no write(batch) method"),
            "{error}"
        );
    }

    let (mut py_sink, _) = sink("Collect");
    let tensors = SourceSchema::Tensor {
        dtype: moruna_kernel::DType::F32,
        shape: vec![-1, 2],
    };
    assert!(
        py_sink
            .open(&tensors)
            .unwrap_err()
            .to_string()
            .contains("takes tables")
    );
    assert!(
        py_sink
            .resume(&tensors, b"", None)
            .unwrap_err()
            .to_string()
            .contains("takes tables")
    );

    let (py_sink, _) = sink("CheckpointFlips");
    assert!(py_sink.checkpoint().expect("the first").is_some());
    let error = py_sink.checkpoint().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("returned None after returning bytes"),
        "{error}"
    );

    let (py_sink, _) = sink("CheckpointNotBytes");
    let error = py_sink.checkpoint().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("returned int, not bytes or None"),
        "{error}"
    );

    let (py_sink, _) = sink("CheckpointRaises");
    let error = py_sink.checkpoint().unwrap_err();
    assert!(
        error.to_string().contains("RuntimeError: no state to give"),
        "{error}"
    );

    let (state_of, _) = sink("NoRestore");
    let state = state_of.checkpoint().expect("checkpoint").expect("bytes");
    let (mut py_sink, _) = sink("NoRestore");
    let error = py_sink
        .resume(&table_source_schema(), &state, None)
        .unwrap_err();
    assert!(matches!(error, MorunaError::Resume(_)), "{error}");
    assert!(
        error.to_string().contains("defines no restore(state)"),
        "{error}"
    );

    let (state_of, _) = sink("Durable");
    let state = state_of.checkpoint().expect("checkpoint").expect("bytes");
    let (mut py_sink, _) = sink("RestoreRaises");
    let error = py_sink
        .resume(&table_source_schema(), &state, None)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("RestoreRaises.restore(): ValueError: corrupt state"),
        "{error}"
    );
}
