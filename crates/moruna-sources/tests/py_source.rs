//! The SO tests of `architecture/sdd/07-sources.md` section k for `PySource` (e.6, f.7): SO-T17 to
//! SO-T22.
//!
//! Each fake is a Python class written inline and imported under a module name of its own, run in
//! the interpreter `PYO3_PYTHON` names (`MORUNA_PYTHON` in the gate), which must import pyarrow.
//! Their own binary, built only with the `python` feature, so a default build links nothing of
//! the interpreter.

#![cfg(feature = "python")]
// `MorunaError` is the contract's and large (CT-I10); the helpers return it as the crate does.
#![allow(clippy::result_large_err)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::arrow::array::{Array, Int64Array, StringArray};
use moruna_kernel::{
    Allocator, MorunaError, Payload, Reactor, RowRange, Source, SourceSchema, Split, Tier,
};
use moruna_sources::PySource;
use moruna_testkit::{FakeAllocator, FakeReactor};
use pyo3::prelude::*;

use support::{CountingAllocator, block_on};

/// The fakes. `Table` holds one batch per split and slices it for a read, which is the natural
/// way a user writes `read`, so the compaction of f.7 is on the path of every test.
const FAKES: &str = r#"
import pyarrow as pa


class Split:
    def __init__(self, id, rows, bytes=None):
        self.id = id
        self.rows = rows
        self.bytes = bytes


def batch(split, start, end):
    values = pa.array(range(split * 100_000 + start, split * 100_000 + end), pa.int64())
    text = pa.array([f"s{split}-r{r}" for r in range(start, end)], pa.string())
    return pa.record_batch({"value": values, "text": text})


class Table:
    def __init__(self, rows, bytes=None, declare=False):
        self.data = {i: batch(i, 0, n) for i, n in enumerate(rows)}
        self.bytes = bytes
        self.declare = declare
        self.reads = []

    def plan(self):
        return [Split(i, b.num_rows, self.bytes) for i, b in self.data.items()]

    def schema(self):
        return self.data[0].schema if self.declare else None

    def read(self, split_id, start, end):
        self.reads.append((split_id, start, end))
        return self.data[split_id].slice(start, end - start)


class NotRepeatable(Table):
    repeatable = False


class WrongRows(Table):
    def read(self, split_id, start, end):
        return self.data[split_id].slice(start, max(end - start - 1, 0))


class Drifts(Table):
    def read(self, split_id, start, end):
        b = self.data[split_id].slice(start, end - start)
        if start == 0:
            return b
        return pa.record_batch({"value": b.column(0).cast(pa.float64()), "text": b.column(1)})


class Renames(Table):
    def read(self, split_id, start, end):
        b = self.data[split_id].slice(start, end - start)
        if start == 0:
            return b
        return pa.record_batch({"v": b.column(0), "text": b.column(1)})


class Raises(Table):
    def read(self, split_id, start, end):
        if start > 0:
            raise ValueError(f"no rows past {start} today")
        return super().read(split_id, start, end)


class NotABatch(Table):
    def read(self, split_id, start, end):
        if start == 0:
            return super().read(split_id, start, end)
        return [1, 2, 3]


class PlanRaises:
    def plan(self):
        raise RuntimeError("the catalogue is down")

    def read(self, split_id, start, end):
        return None


class PlanIsNotAList:
    def plan(self):
        return 7

    def read(self, split_id, start, end):
        return None


class PlanHasNoRows:
    def plan(self):
        return [object()]

    def read(self, split_id, start, end):
        return None


class PlanNegative:
    def plan(self):
        return [Split(0, -4)]

    def read(self, split_id, start, end):
        return None


class PlanTwice(Table):
    def plan(self):
        return [Split(3, 10), Split(3, 10)]


class Empty:
    def __init__(self, declare):
        self.declare = declare

    def plan(self):
        return []

    def schema(self):
        return pa.schema([("value", pa.int64())]) if self.declare else None

    def read(self, split_id, start, end):
        raise AssertionError("an empty plan is never read")


class SchemaRaises(Table):
    def schema(self):
        raise KeyError("schema")


class SchemaIsNotOne(Table):
    def schema(self):
        return "value: int64"


class BadRepeatable(Table):
    repeatable = "sometimes"


class DeclaredWrong(Table):
    def schema(self):
        return pa.schema([("value", pa.string()), ("text", pa.string())])
"#;

/// A new instance of fake `class` built by the Python expression `args` (e.g. `"[30, 0, 7]"`).
fn fake(class: &str, args: &str) -> Py<PyAny> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // The test threads build their fakes one at a time: two threads importing pyarrow at once
    // under the free-threaded interpreter can see the module half initialised.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    Python::initialize();
    Python::attach(|py| {
        let name = format!("so_py_source_{}", NEXT.fetch_add(1, Ordering::SeqCst));
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
        let expr = format!("{class}({args})");
        let globals = module.dict();
        py.eval(
            std::ffi::CString::new(expr)
                .expect("an expression")
                .as_c_str(),
            Some(&globals),
            None,
        )
        .expect("the fake builds")
        .unbind()
    })
}

fn reactor() -> Arc<dyn Reactor> {
    Arc::new(FakeReactor::new()) as Arc<dyn Reactor>
}

fn planned(class: &str, args: &str) -> (PySource, Py<PyAny>) {
    let object = fake(class, args);
    let clone = Python::attach(|py| object.clone_ref(py));
    (
        PySource::new(object, reactor()).expect("the source plans"),
        clone,
    )
}

fn refused(class: &str, args: &str) -> MorunaError {
    match PySource::new(fake(class, args), reactor()) {
        Ok(_) => panic!("{class}({args}) was accepted"),
        Err(e) => e,
    }
}

/// The reads the fake recorded, as `(split, start, end)`.
fn reads(object: &Py<PyAny>) -> Vec<(u32, u64, u64)> {
    Python::attach(|py| {
        object
            .bind(py)
            .getattr("reads")
            .expect("reads")
            .extract()
            .expect("a list of triples")
    })
}

fn values(payload: &Payload) -> (Vec<i64>, Vec<String>) {
    let Payload::Table(batch, _) = payload else {
        panic!("a table payload");
    };
    let ints = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("int64");
    let texts = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("utf8");
    (
        ints.values().to_vec(),
        (0..texts.len())
            .map(|i| texts.value(i).to_string())
            .collect(),
    )
}

fn expected(split: u32, start: u64, end: u64) -> (Vec<i64>, Vec<String>) {
    (
        (start..end)
            .map(|r| split as i64 * 100_000 + r as i64)
            .collect(),
        (start..end).map(|r| format!("s{split}-r{r}")).collect(),
    )
}

fn read(
    source: &PySource,
    split: &Split,
    rows: Option<RowRange>,
    alloc: &CountingAllocator,
) -> moruna_kernel::Result<Payload> {
    block_on(source.read(split, rows, alloc, Tier::Host))
}

fn range(start: u64, end: u64) -> Option<RowRange> {
    Some(RowRange { start, end })
}

// SO-T17 py_source_plan_and_read. e.6, f.7, SO-I2, SO-I3, SO-I5.
#[test]
fn so_t17_py_source_plan_and_read() {
    let (source, object) = planned("Table", "[3000, 0, 7]");
    let plan = source.plan().expect("the plan");
    assert_eq!(
        plan.iter().map(|s| (s.id, s.rows)).collect::<Vec<_>>(),
        vec![(0, 3000), (1, 0), (2, 7)],
        "ids and row counts are the user's"
    );
    for split in &plan {
        assert!(split.estimated, "the bytes are an estimate");
        assert!(split.sub_splittable, "read takes a range");
        assert_eq!(split.column_bytes.len(), 2);
        assert_eq!(split.null_counts, vec![None, None]);
        assert_eq!(
            split.uncompressed_bytes,
            split.column_bytes.iter().sum::<u64>()
        );
    }
    assert!(plan[0].uncompressed_bytes > plan[2].uncompressed_bytes);
    assert_eq!(plan[1].uncompressed_bytes, 0);
    // The schema came from one sample read of the first non-empty split.
    assert_eq!(reads(&object), vec![(0, 0, 1024)]);
    let SourceSchema::Table(schema) = source.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.field(0).name(), "value");
    assert!(source.repeatable());

    let alloc = CountingAllocator::new(FakeAllocator::new());
    for split in &plan {
        let payload = read(&source, split, None, &alloc).expect("a whole split reads");
        assert_eq!(payload.rows() as u64, split.rows);
        assert_eq!(values(&payload), expected(split.id, 0, split.rows));
        let Payload::Table(batch, _) = &payload else {
            panic!("a table payload");
        };
        for column in batch.columns() {
            for buffer in column.to_data().buffers() {
                assert!(
                    alloc.contains(buffer.as_ptr()),
                    "SO-I3: every buffer is arena-owned"
                );
            }
        }
    }
    let stats = source.stats();
    assert_eq!(stats.splits, 3);
    assert_eq!(stats.reads, 3);
    assert_eq!(stats.decode_bytes, alloc.payload_copy_bytes(), "SO-I5");
    assert_eq!(alloc.payload_copies(), 3, "one decode copy per read");
    assert_eq!(
        reads(&object)[1..],
        [(0, 0, 3000), (1, 0, 0), (2, 0, 7)],
        "a whole read asks for the whole split"
    );
}

// SO-T18 py_source_sub_split. SO-I4, SO-I6, f.7 (compaction).
#[test]
fn so_t18_py_source_sub_split() {
    let (source, object) = planned("Table", "[40000]");
    let split = source.plan().expect("the plan").remove(0);
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let mut ints = Vec::new();
    let mut texts = Vec::new();
    for (start, end) in [(0, 10), (10, 25), (25, 40000)] {
        let payload = read(&source, &split, range(start, end), &alloc).expect("a range reads");
        assert_eq!(
            payload.rows() as u64,
            end - start,
            "SO-I4: exactly the range"
        );
        let (i, t) = values(&payload);
        ints.extend(i);
        texts.extend(t);
    }
    assert_eq!(
        (ints, texts),
        expected(0, 0, 40000),
        "ranges concatenate to the split"
    );

    // SO-I6: the same range twice, and in reverse order, reads the same rows.
    let later = read(&source, &split, range(30000, 30010), &alloc).expect("a range reads");
    let earlier = read(&source, &split, range(5, 15), &alloc).expect("a range reads");
    let again = read(&source, &split, range(30000, 30010), &alloc).expect("a range reads");
    assert_eq!(values(&later), values(&again));
    assert_eq!(values(&earlier), expected(0, 5, 15));

    // f.7: a ten-row slice of a 40,000-row batch puts ten rows in the arena, not the batch.
    let before = alloc.payload_copy_bytes();
    read(&source, &split, range(100, 110), &alloc).expect("a range reads");
    let copied = alloc.payload_copy_bytes() - before;
    assert!(
        copied < split.uncompressed_bytes / 100,
        "a slice was copied whole: {copied} bytes for 10 of 40000 rows ({} planned)",
        split.uncompressed_bytes
    );
    assert!(
        source.stats().compacted_bytes > 0,
        "the slice was compacted"
    );
    assert!(reads(&object).contains(&(0, 100, 110)));
}

// SO-T19 py_source_wrong_rows. h, SO-I4: a batch with the wrong row count, or a schema that
// differs from the first, is a `Source` error naming the split and the figures.
#[test]
fn so_t19_py_source_wrong_rows() {
    let alloc = CountingAllocator::new(FakeAllocator::new());

    // The sample read at plan time is checked too.
    let error = refused("WrongRows", "[50]");
    assert!(
        matches!(error, MorunaError::Source { split: 0, .. }),
        "{error}"
    );
    let text = error.to_string();
    assert!(text.contains("returned 49 rows"), "{text}");
    assert!(text.contains("exactly the 50 rows of [0, 50)"), "{text}");

    let (source, _) = planned("WrongRows", "[50], bytes=4096, declare=True");
    let split = source.plan().expect("the plan").remove(0);
    let error = read(&source, &split, range(10, 20), &alloc).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 0, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("returned 9 rows"), "{error}");

    let (source, _) = planned("Drifts", "[50]");
    let split = source.plan().expect("the plan").remove(0);
    read(&source, &split, range(0, 10), &alloc).expect("the first range is the schema");
    let error = read(&source, &split, range(10, 20), &alloc).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 0, .. }),
        "{error}"
    );
    let text = error.to_string();
    assert!(text.contains("schema differs"), "{text}");
    assert!(
        text.contains("value: Int64") && text.contains("value: Float64"),
        "{text}"
    );

    let (source, _) = planned("Renames", "[50]");
    let split = source.plan().expect("the plan").remove(0);
    let error = read(&source, &split, range(10, 20), &alloc).unwrap_err();
    assert!(error.to_string().contains("v: Int64"), "{error}");

    // A declared schema the batches do not have is refused at the sample.
    let error = refused("DeclaredWrong", "[50], declare=True");
    assert!(error.to_string().contains("schema differs"), "{error}");

    let (source, _) = planned("NotABatch", "[50]");
    let split = source.plan().expect("the plan").remove(0);
    let error = read(&source, &split, range(10, 20), &alloc).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("returned list, not a pyarrow.RecordBatch"),
        "{error}"
    );
}

// SO-T20 py_source_exception. h: an exception in user code is a `Source` error (in `read`) or a
// `Plan` error (in `plan` and `schema`) carrying the Python message.
#[test]
fn so_t20_py_source_exception() {
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let (source, _) = planned("Raises", "[50, 60]");
    let split = source.plan().expect("the plan").remove(1);
    let error = read(&source, &split, range(20, 30), &alloc).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 1, .. }),
        "{error}"
    );
    let text = error.to_string();
    assert!(text.contains("ValueError: no rows past 20 today"), "{text}");
    assert!(text.contains("Raises.read(1, 20, 30)"), "{text}");
    assert_eq!(
        alloc.fake().allocations_total(),
        0,
        "nothing reached the arena"
    );

    for (class, args, needle) in [
        ("PlanRaises", "", "RuntimeError: the catalogue is down"),
        (
            "PlanIsNotAList",
            "",
            "returned int, not a list of moruna.Split",
        ),
        ("PlanHasNoRows", "", "which has no `id`"),
        (
            "PlanNegative",
            "",
            "rows must be a non-negative int, not -4",
        ),
        ("PlanTwice", "[1]", "names split 3 twice"),
        ("SchemaRaises", "[5]", "KeyError: 'schema'"),
        (
            "SchemaIsNotOne",
            "[5]",
            "returned str, not a pyarrow.Schema",
        ),
        ("BadRepeatable", "[5]", "repeatable must be True or False"),
        ("Empty", "False", "must declare its schema"),
    ] {
        let error = refused(class, args);
        assert!(matches!(error, MorunaError::Plan(_)), "{class}: {error}");
        assert!(error.to_string().contains(needle), "{class}: {error}");
    }
}

// SO-T21 py_source_not_repeatable. SO-I8: `repeatable = False` turns resume and Q0 eviction off,
// through `repeatable()`, the one method the facade reads for both (12 f.1); the facade's side is
// `crates/moruna-runtime/tests/lifecycle.rs` rt_t3 and `python/tests/test_custom_source_sink.py`.
#[test]
fn so_t21_py_source_not_repeatable() {
    let (source, _) = planned("NotRepeatable", "[20]");
    assert!(!source.repeatable());
    let split = source.plan().expect("the plan").remove(0);
    assert!(split.sub_splittable, "ranges still work");
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let payload = read(&source, &split, range(5, 9), &alloc).expect("a range reads");
    assert_eq!(values(&payload), expected(0, 5, 9));
}

// SO-T22 py_source_plan_edges. SO-I1, SO-I7, h: a declared schema and byte estimate cost no read
// at plan time; an empty plan with a schema is a valid source; an unplanned id and a range
// outside the split are `Source` errors.
#[test]
fn so_t22_py_source_plan_edges() {
    let (source, object) = planned("Table", "[100, 50], bytes=8000, declare=True");
    assert!(reads(&object).is_empty(), "nothing to learn, nothing read");
    let plan = source.plan().expect("the plan");
    assert_eq!(
        plan[0].uncompressed_bytes, 8000,
        "the user's estimate is kept"
    );
    assert_eq!(
        plan[0].column_bytes,
        vec![4000, 4000],
        "an even share without a sample"
    );

    // With a byte estimate and a sample, the estimate is shared by the sample's proportions.
    let (source, _) = planned("Table", "[100], bytes=8000");
    let split = source.plan().expect("the plan").remove(0);
    assert_eq!(split.uncompressed_bytes, 8000);
    assert_eq!(split.column_bytes.len(), 2);
    assert!(split.column_bytes.iter().sum::<u64>().abs_diff(8000) <= 1);

    // Every split empty: the zero-row read of the first is the schema.
    let (source, object) = planned("Table", "[0, 0]");
    assert_eq!(reads(&object), vec![(0, 0, 0)]);
    assert!(matches!(source.schema(), SourceSchema::Table(_)));

    let (source, _) = planned("Empty", "True");
    assert!(source.plan().expect("the plan").is_empty());
    let SourceSchema::Table(schema) = source.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.fields().len(), 1);

    let (source, _) = planned("Table", "[10]");
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let mut stranger = source.plan().expect("the plan").remove(0);
    stranger.id = 9;
    let error = read(&source, &stranger, None, &alloc).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 9, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("not in Table's plan"), "{error}");
    let split = source.plan().expect("the plan").remove(0);
    let error = read(&source, &split, range(5, 11), &alloc).unwrap_err();
    assert!(
        error.to_string().contains("outside split 0 of 10 rows"),
        "{error}"
    );
    let error = read(&source, &split, range(6, 5), &alloc).unwrap_err();
    assert!(error.to_string().contains("outside split 0"), "{error}");

    // The arena's refusal is passed through as it is (h).
    let tight = CountingAllocator::new(FakeAllocator::new().fail_next(1));
    let error = read(&source, &split, None, &tight).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");
}
