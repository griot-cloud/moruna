//! SC-T47b (item 6.3a, system design 3A): a Parquet row group cut to fit the arena.
//!
//! The drive cuts a read that does not fit beside those in flight to the room left, from a row
//! group whose pages can be located (`ParquetSource::range_reads_own_rows`). The parts of one
//! group share the group's one fetch (`keep_fetched`) and each decodes only the pages it covers,
//! and every row arrives exactly once: the same rows, counted and hashed, as when every group is
//! read whole. A group whose projection holds a repeated column cannot have its pages located,
//! so it is read whole, as before.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::array::{
    Array, ArrayRef, Float64Array, Int32Array, Int64Array, ListArray, RecordBatch, StringArray,
};
use moruna_kernel::arrow::datatypes::Int32Type;
use moruna_kernel::{
    BoxFuture, CancelToken, Payload, PayloadSpec, Result, Seq, Sink, SinkSummary, Source,
    SourceSchema, Tier,
};
use moruna_sources::{ParquetSource, ParquetSourceConfig};
use moruna_testkit::{FakeAllocator, FakeReactor, FakeSink};

use super::common::RigBuilder;

/// Row groups in the file, and rows in each: pages of 512 rows, so 32 pages a column.
const GROUPS: u64 = 6;
const GROUP_ROWS: u64 = 16_384;

/// A Parquet file of `GROUPS` row groups: a row id, a nullable float, a short string and, when
/// `repeated`, a list column. Small pages, zstd, no page index (pyarrow's default too).
fn write_file(dir: &Path, repeated: bool) -> PathBuf {
    std::fs::create_dir_all(dir).expect("the test directory");
    let path = dir.join(if repeated {
        "repeated.parquet"
    } else {
        "flat.parquet"
    });
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(GROUP_ROWS as usize))
        .set_data_page_row_count_limit(512)
        .set_write_batch_size(512)
        .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
        .build();
    let mut writer = None;
    for group in 0..GROUPS {
        let rows = group * GROUP_ROWS..(group + 1) * GROUP_ROWS;
        let mut columns: Vec<(&str, ArrayRef)> = vec![
            (
                "id",
                Arc::new(Int64Array::from_iter_values(rows.clone().map(|r| r as i64))),
            ),
            (
                "fare",
                Arc::new(Float64Array::from_iter(
                    rows.clone()
                        .map(|r| (r % 7 != 0).then_some(r as f64 * 0.25)),
                )),
            ),
            (
                "zone",
                Arc::new(StringArray::from_iter_values(
                    rows.clone().map(|r| format!("z{}", r % 263)),
                )),
            ),
        ];
        if repeated {
            columns.push((
                "tags",
                Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
                    rows.clone()
                        .map(|r| Some((0..(r % 3) as i32).map(Some).collect::<Vec<_>>())),
                )),
            ));
        }
        let batch = RecordBatch::try_from_iter(columns).expect("the batch");
        let writer = writer.get_or_insert_with(|| {
            let file = std::fs::File::create(&path).expect("the parquet file");
            parquet::arrow::ArrowWriter::try_new(file, batch.schema(), Some(properties.clone()))
                .expect("the parquet writer")
        });
        writer.write(&batch).expect("writing the batch");
        writer.flush().expect("closing the row group");
    }
    writer.expect("a group").close().expect("closing the file");
    path
}

/// What reached the sink: how many times each row id arrived, and a hash of the multiset of
/// rows (every column), the gate's canonical form (bench/vm/canon.py) in miniature.
#[derive(Default, Debug, PartialEq, Eq)]
struct Arrived {
    seen: Vec<u32>,
    rows: u64,
    hash: u128,
}

/// A sink that records what `Arrived` needs of each payload, then lets the fake take it.
struct Hashing {
    inner: FakeSink,
    arrived: Arc<Mutex<Arrived>>,
}

impl Hashing {
    fn note(&self, payload: &Payload) -> Result<()> {
        let Payload::Table(batch, _) = payload else {
            return Err(moruna_kernel::MorunaError::Plan(
                "a table was expected".into(),
            ));
        };
        let column = |name: &str| batch.column_by_name(name).cloned();
        let (id, fare, zone) = (
            column("id").expect("ids"),
            column("fare").expect("fares"),
            column("zone").expect("zones"),
        );
        let tags = column("tags");
        let id = id.as_any().downcast_ref::<Int64Array>().expect("int64 ids");
        let fare = fare
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("float64");
        let zone = zone
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("strings");
        let tags = tags
            .as_ref()
            .map(|t| t.as_any().downcast_ref::<ListArray>().expect("a list"));
        let mut arrived = self.arrived.lock().unwrap_or_else(|e| e.into_inner());
        for row in 0..id.len() {
            let mut hasher = DefaultHasher::new();
            id.value(row).hash(&mut hasher);
            fare.is_valid(row)
                .then(|| fare.value(row).to_bits())
                .hash(&mut hasher);
            zone.value(row).hash(&mut hasher);
            if let Some(tags) = tags {
                let values = tags.value(row);
                let values = values.as_any().downcast_ref::<Int32Array>().expect("int32");
                values.values().hash(&mut hasher);
            }
            arrived.hash = arrived.hash.wrapping_add(u128::from(hasher.finish()));
            arrived.rows += 1;
            let at = usize::try_from(id.value(row)).expect("a row id");
            arrived.seen[at] += 1;
        }
        Ok(())
    }
}

impl Sink for Hashing {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        if let Err(e) = self.note(&payload) {
            return Box::pin(async move { Err(e) });
        }
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.inner.skip(seq);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }
}

/// What one copy of the file saw.
struct Copied {
    arrived: Arrived,
    stats: moruna_sources::stats::SourceStats,
    charged_peak: u64,
}

/// A source over `path`, decoding in batches of at most `batch` bytes when given (3A).
fn source(path: &Path, batch: Option<u64>) -> Arc<ParquetSource> {
    let url = path.display().to_string();
    let reactor = FakeReactor::new().with_file(&url, std::fs::read(path).expect("the file"));
    Arc::new(
        ParquetSource::new(
            ParquetSourceConfig {
                urls: vec![url],
                ..ParquetSourceConfig::default()
            },
            Arc::new(reactor.clone()),
            Arc::new(reactor),
        )
        .expect("the source")
        .decode_batches_of(batch),
    )
}

/// The largest row group's estimate: what the drive charges twice for a whole read of it.
fn largest_group(path: &Path) -> u64 {
    let plan = source(path, None).plan().expect("the plan");
    assert_eq!(plan.len() as u64, GROUPS);
    plan.iter()
        .map(|split| split.uncompressed_bytes)
        .max()
        .unwrap_or(0)
}

/// Copies the file with two workers and a read-ahead of two into `alloc`, each read a whole
/// group unless the drive cuts it, decoding in batches of `batch` bytes when given.
fn copy(path: &Path, alloc: FakeAllocator, batch: Option<u64>) -> Copied {
    let parquet = source(path, batch);
    let arrived = Arc::new(Mutex::new(Arrived {
        seen: vec![0; (GROUPS * GROUP_ROWS) as usize],
        ..Arrived::default()
    }));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.read_ahead = 2;
            cfg.initial_morsel_target = 1 << 30;
            cfg.morsel_max = 1 << 30;
        })
        .source_over(Arc::clone(&parquet) as Arc<dyn Source>)
        .sink_over(Box::new(Hashing {
            inner: FakeSink::new(),
            arrived: Arc::clone(&arrived),
        }))
        .alloc(alloc.clone())
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let charged_peak = rig.scheduler.shared().sources[0]
        .charged_peak
        .load(std::sync::atomic::Ordering::SeqCst);
    drop(rig);
    assert_eq!(alloc.in_use(Tier::Host), 0, "every read was released");
    let arrived = std::mem::take(&mut *arrived.lock().unwrap_or_else(|e| e.into_inner()));
    assert!(
        arrived.seen.iter().all(|&n| n == 1),
        "every row arrived exactly once"
    );
    Copied {
        arrived,
        stats: parquet.stats(),
        charged_peak,
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("moruna-sc-t47-{}-{name}", std::process::id()))
}

#[test]
fn sc_t47_parts_of_a_row_group_share_its_fetch_and_decode_their_own_pages() {
    let dir = scratch("flat");
    let path = write_file(&dir, false);
    let whole = copy(&path, FakeAllocator::new(), None);
    assert_eq!(whole.stats.group_fetches, GROUPS);
    assert_eq!(
        whole.stats.ranges_decoded, 0,
        "with no budget every group is read whole"
    );

    // Room for one whole group at its charge, never two: the second read is cut to the room,
    // and every read is decoded in batches of a page's rows, each copied into the arena as it
    // comes.
    let arena = 7 * largest_group(&path) / 2;
    let cut = copy(
        &path,
        FakeAllocator::new().with_limit(Tier::Host, arena),
        Some(1),
    );
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        cut.stats.ranges_decoded > 0,
        "an arena of {arena} bytes cut reads: {:?}",
        cut.stats
    );
    assert_eq!(
        cut.stats.group_fetches, GROUPS,
        "the parts of a group share its one fetch: {:?}",
        cut.stats
    );
    assert_eq!(
        cut.stats.ranges_paged, cut.stats.ranges_decoded,
        "every part decoded only its own pages: {:?}",
        cut.stats
    );
    assert!(cut.charged_peak <= arena, "{} > {arena}", cut.charged_peak);
    assert!(
        cut.stats.batches_decoded >= GROUPS * GROUP_ROWS / 512,
        "decoded a page's rows at a time: {:?}",
        cut.stats
    );
    assert_eq!(cut.arrived.rows, GROUPS * GROUP_ROWS);
    assert_eq!(
        cut.arrived, whole.arrived,
        "the same rows as the groups read whole, each once"
    );
}

#[test]
fn sc_t47_a_row_group_with_a_repeated_column_is_read_whole() {
    let dir = scratch("repeated");
    let path = write_file(&dir, true);
    let parquet = source(&path, None);
    let plan = parquet.plan().expect("the plan");
    assert!(
        plan.iter()
            .all(|split| split.sub_splittable && !parquet.range_reads_own_rows(split)),
        "a list column's pages cannot be located from their headers"
    );

    let whole = copy(&path, FakeAllocator::new(), None);
    let arena = 7 * largest_group(&path) / 2;
    let tight = copy(
        &path,
        FakeAllocator::new().with_limit(Tier::Host, arena),
        Some(1),
    );
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        (tight.stats.reads, tight.stats.ranges_decoded),
        (GROUPS, 0),
        "today's behaviour: every group read whole, uncut: {:?}",
        tight.stats
    );
    assert_eq!(tight.arrived, whole.arrived);
}
