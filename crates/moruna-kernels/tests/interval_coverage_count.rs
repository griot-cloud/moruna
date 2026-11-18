//! `moruna.interval_coverage_count@1` (4B): hand-checked cells, the midnight wrap, every rule of
//! its specification, the merge, the kernel contract, and agreement with Occupancy's
//! expand-and-count (`bench/vm/workloads.py`) on drawn trips.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::array::{Array, AsArray, Int32Array, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Int64Type, Schema};
use moruna_kernel::payload::DType;
use moruna_kernel::{
    InitCtx, Kernel, KernelKind, Level, NoState, OutputSink, Payload, SchemaDecl, SourceSchema,
    Stream,
};
use moruna_kernels::catalogue::{self, Params};
use moruna_kernels::conformance::{Expect, Step, read_corpus, read_source};
use moruna_kernels::interval_coverage_count::{
    self as icc, DENSE_CELLS_LIMIT, Grid, IntervalCoverageCount, NAME, Notes, OUTPUT_COLUMNS,
    VERSION, coverage, identity, merge, output_schema,
};
use moruna_testkit::FakeAllocator;
use moruna_testkit::dag::Rng;
use serde_json::{Value, json};

type Cell = (i32, i32, i64);

fn cells(batch: &RecordBatch) -> Vec<Cell> {
    assert_eq!(batch.schema(), output_schema());
    let g = batch.column(0).as_primitive::<Int32Type>();
    let b = batch.column(1).as_primitive::<Int32Type>();
    let c = batch.column(2).as_primitive::<Int64Type>();
    (0..batch.num_rows())
        .map(|i| (g.value(i), b.value(i), c.value(i)))
        .collect()
}

fn run(grid: &Grid, rows: &[(Option<i32>, Option<i64>, Option<i64>)]) -> (Vec<Cell>, Notes) {
    let group: Int32Array = rows.iter().map(|r| r.0).collect();
    let start: Int64Array = rows.iter().map(|r| r.1).collect();
    let length: Int64Array = rows.iter().map(|r| r.2).collect();
    let (out, notes) = coverage(grid, &group, &start, &length).expect("coverage");
    (cells(&out), notes)
}

fn some(rows: &[(i32, i64, i64)]) -> Vec<(Option<i32>, Option<i64>, Option<i64>)> {
    rows.iter()
        .map(|&(g, s, l)| (Some(g), Some(s), Some(l)))
        .collect()
}

fn grid(step: i64, size: i64) -> Grid {
    Grid::new(step, size, None).expect("grid")
}

/// Expand-and-count, as Occupancy as written does it: every interval into its buckets, then
/// counted per (group, bucket).
fn expand_and_count(grid: &Grid, rows: &[(i32, i64, i64)]) -> Vec<Cell> {
    let mut counts: BTreeMap<(i32, i32), i64> = BTreeMap::new();
    for &(g, s, l) in rows {
        let first = s.div_euclid(grid.step());
        for k in 0..l.max(0) {
            let bucket = (first + k).rem_euclid(i64::from(grid.size())) as i32;
            *counts.entry((g, bucket)).or_default() += 1;
        }
    }
    counts.into_iter().map(|((g, b), c)| (g, b, c)).collect()
}

#[test]
fn hand_checked_cells_in_key_order() {
    // A grid of 10 buckets, 5 units wide. Group 2: [3, 7) and [5, 9) in buckets; group 1: one
    // interval of one bucket at position 14 (bucket 2).
    let g = grid(5, 10);
    let (out, notes) = run(&g, &some(&[(2, 15, 4), (2, 25, 4), (1, 14, 1)]));
    assert_eq!(
        out,
        vec![
            (1, 2, 1),
            (2, 3, 1),
            (2, 4, 1),
            (2, 5, 2),
            (2, 6, 2),
            (2, 7, 1),
            (2, 8, 1),
        ]
    );
    assert!(notes.is_empty());
    assert_eq!(notes.text(&g), None);
}

#[test]
fn an_interval_across_midnight_wraps_to_the_first_buckets() {
    // A day of minutes in microseconds: 23:58 for 5 minutes covers 23:58, 23:59, 00:00, 00:01
    // and 00:02.
    let g = grid(60_000_000, 1440);
    let day = 86_400_000_000i64;
    let at = |h: i64, m: i64, s: i64| 19_723 * day + ((h * 60 + m) * 60 + s) * 1_000_000;
    let (out, _) = run(&g, &some(&[(7, at(23, 58, 30), 5)]));
    assert_eq!(
        out,
        vec![(7, 0, 1), (7, 1, 1), (7, 2, 1), (7, 1438, 1), (7, 1439, 1)]
    );
    // Ending exactly at midnight does not wrap: 23:58 for 2 minutes.
    let (out, _) = run(&g, &some(&[(7, at(23, 58, 0), 2)]));
    assert_eq!(out, vec![(7, 1438, 1), (7, 1439, 1)]);
    // Starting at midnight: 00:00:00 for 1 minute.
    let (out, _) = run(&g, &some(&[(7, at(0, 0, 0), 1)]));
    assert_eq!(out, vec![(7, 0, 1)]);
    // Two trips around midnight in one zone, one in another, the cells in key order.
    let (out, _) = run(
        &g,
        &some(&[
            (3, at(23, 59, 59), 3),
            (3, at(0, 0, 1), 2),
            (1, at(23, 59, 0), 2),
        ]),
    );
    assert_eq!(
        out,
        vec![(1, 0, 1), (1, 1439, 1), (3, 0, 2), (3, 1, 2), (3, 1439, 1)]
    );
}

#[test]
fn an_interval_longer_than_the_grid_counts_once_per_pass() {
    // Four buckets; 10 buckets from bucket 3 pass 3, 0, 1, 2, 3, 0, 1, 2, 3, 0.
    let g = grid(1, 4);
    let (out, _) = run(&g, &some(&[(0, 3, 10)]));
    assert_eq!(out, vec![(0, 0, 3), (0, 1, 2), (0, 2, 2), (0, 3, 3)]);
    assert_eq!(out, expand_and_count(&g, &[(0, 3, 10)]));
    // Exactly the grid: every bucket once.
    let (out, _) = run(&g, &some(&[(0, 2, 4)]));
    assert_eq!(out, vec![(0, 0, 1), (0, 1, 1), (0, 2, 1), (0, 3, 1)]);
    // A grid of one bucket: the length is the count.
    let (out, _) = run(&grid(7, 1), &some(&[(5, -100, 9), (5, 3, 1)]));
    assert_eq!(out, vec![(5, 0, 10)]);
}

#[test]
fn a_zero_or_negative_length_covers_nothing() {
    let g = grid(1, 8);
    let (out, notes) = run(&g, &some(&[(1, 2, 0), (1, 3, -1), (1, 4, i64::MIN)]));
    assert!(out.is_empty());
    assert!(notes.is_empty());
    let (out, _) = run(&g, &some(&[(1, 2, 0), (1, 3, 1)]));
    assert_eq!(out, vec![(1, 3, 1)]);
}

#[test]
fn a_negative_start_is_floored() {
    // Step 10, size 6: position -1 is in bucket floor(-1/10) = -1, which is 5 on the ring;
    // -60 is -6, which is 0; -61 is -7, which is 5.
    let g = grid(10, 6);
    let (out, _) = run(&g, &some(&[(0, -1, 2), (1, -60, 1), (2, -61, 1)]));
    assert_eq!(out, vec![(0, 0, 1), (0, 5, 1), (1, 0, 1), (2, 5, 1)]);
    assert_eq!(
        g.bucket(i64::MIN),
        (i64::MIN.div_euclid(10)).rem_euclid(6) as i32
    );
    assert_eq!(
        grid(1, i64::from(i32::MAX)).bucket(i64::MAX),
        (i64::MAX % i64::from(i32::MAX)) as i32
    );
}

#[test]
fn a_null_skips_its_row_and_is_noted() {
    let g = grid(1, 4);
    let rows = vec![
        (None, Some(0), Some(1)),
        (Some(1), None, Some(1)),
        (Some(1), Some(1), None),
        (Some(1), Some(2), Some(1)),
    ];
    let (out, notes) = run(&g, &rows);
    assert_eq!(out, vec![(1, 2, 1)]);
    assert_eq!(notes.skipped_nulls, 3);
    assert_eq!(notes.outside_domain, 0);
    assert!(!notes.is_empty());
    let text = notes.text(&g).expect("a note");
    assert!(
        text.starts_with("moruna.interval_coverage_count@1: 3 rows skipped for a null"),
        "{text}"
    );
}

#[test]
fn a_declared_domain_is_dense_and_a_group_outside_it_goes_sparse_with_the_same_cells() {
    let rows = [(1, 0, 3), (3, 2, 4), (2, 7, 2), (1, 5, 1)];
    let sparse = grid(1, 8);
    let dense = Grid::new(1, 8, Some((0, 3))).expect("dense");
    let (want, _) = run(&sparse, &some(&rows));
    let (got, notes) = run(&dense, &some(&rows));
    assert_eq!(got, want);
    assert!(notes.is_empty());
    // A domain that leaves out group 3: the batch goes sparse, with the same cells, and says so.
    let narrow = Grid::new(1, 8, Some((1, 2))).expect("narrow");
    let (got, notes) = run(&narrow, &some(&rows));
    assert_eq!(got, want);
    assert_eq!(notes.outside_domain, 1);
    assert_eq!(notes.first_outside, Some((1, 3)));
    let text = notes.text(&narrow).expect("a note");
    assert!(text.contains("sparse table"), "{text}");
    assert!(text.contains("[1, 2]"), "{text}");
    assert!(text.contains("row 1 with group 3"), "{text}");
    // A group below the domain, and a null beside it.
    let mut with_null = some(&[(-5, 0, 1), (1, 0, 1)]);
    with_null.push((None, None, None));
    let (got, notes) = run(&narrow, &with_null);
    assert_eq!(got, vec![(-5, 0, 1), (1, 0, 1)]);
    assert_eq!(
        (
            notes.skipped_nulls,
            notes.outside_domain,
            notes.first_outside
        ),
        (1, 1, Some((0, -5)))
    );
    let text = notes.text(&narrow).expect("a note");
    assert!(
        text.contains("1 rows skipped") && text.contains("; the batch"),
        "{text}"
    );
}

#[test]
fn empty_input_gives_no_cells() {
    for g in [grid(1, 4), Grid::new(1, 4, Some((0, 0))).expect("dense")] {
        let (out, notes) = run(&g, &[]);
        assert!(out.is_empty());
        assert!(notes.is_empty());
        let (out, _) = run(&g, &[(None, None, None)]);
        assert!(out.is_empty());
    }
}

#[test]
fn a_sliced_input_is_read_from_its_offset() {
    let g = grid(1, 4);
    let group = Int32Array::from(vec![9, 1, 2, 9]);
    let start = Int64Array::from(vec![0, 1, 2, 0]);
    let length = Int64Array::from(vec![4, 1, 1, 4]);
    let (out, _) = coverage(
        &g,
        &group.slice(1, 2),
        &start.slice(1, 2),
        &length.slice(1, 2),
    )
    .expect("sliced");
    assert_eq!(cells(&out), vec![(1, 1, 1), (2, 2, 1)]);
    let nulls = Int32Array::from(vec![None, Some(1), None]);
    let (out, notes) = coverage(
        &g,
        &nulls.slice(1, 2),
        &start.slice(1, 2),
        &length.slice(1, 2),
    )
    .expect("sliced nulls");
    assert_eq!(cells(&out), vec![(1, 1, 1)]);
    assert_eq!(notes.skipped_nulls, 1);
}

#[test]
fn inputs_of_different_lengths_are_refused() {
    let g = grid(1, 4);
    let err = coverage(
        &g,
        &Int32Array::from(vec![1]),
        &Int64Array::from(vec![1, 2]),
        &Int64Array::from(vec![1]),
    )
    .expect_err("lengths");
    assert!(err.to_string().contains("one length"), "{err}");
}

#[test]
fn a_count_that_overflows_int64_is_refused() {
    // A grid of one bucket counts each length whole: two of i64::MAX overflow.
    let g = grid(1, 1);
    let rows = some(&[(0, 0, i64::MAX), (0, 0, i64::MAX)]);
    let group: Int32Array = rows.iter().map(|r| r.0).collect();
    let start: Int64Array = rows.iter().map(|r| r.1).collect();
    let length: Int64Array = rows.iter().map(|r| r.2).collect();
    let err = coverage(&g, &group, &start, &length).expect_err("overflow");
    assert!(err.to_string().contains("overflows Int64"), "{err}");
    // Full turns to i64::MAX - 1 in every bucket, then two more in bucket 0: the running sum
    // overflows.
    let g = grid(1, 2);
    let rows = some(&[
        (0, 0, i64::MAX - 1),
        (0, 0, i64::MAX - 1),
        (0, 0, 1),
        (0, 0, 1),
    ]);
    let group: Int32Array = rows.iter().map(|r| r.0).collect();
    let start: Int64Array = rows.iter().map(|r| r.1).collect();
    let length: Int64Array = rows.iter().map(|r| r.2).collect();
    let err = coverage(&g, &group, &start, &length).expect_err("overflow");
    assert!(err.to_string().contains("overflows Int64"), "{err}");
}

#[test]
fn the_grid_is_checked() {
    assert!(Grid::new(0, 4, None).is_err());
    assert!(Grid::new(-1, 4, None).is_err());
    assert!(Grid::new(1, 0, None).is_err());
    assert!(Grid::new(1, i64::from(i32::MAX) + 1, None).is_err());
    assert!(Grid::new(1, i64::from(i32::MAX), None).is_ok());
    assert!(Grid::new(1, 4, Some((2, 1))).is_err());
    let err = Grid::new(1, 1440, Some((0, i32::MAX))).expect_err("too big");
    assert!(err.to_string().contains("dense table"), "{err}");
    // The largest domain that fits.
    let span = (DENSE_CELLS_LIMIT / 1441) as i32;
    assert!(Grid::new(1, 1440, Some((0, span - 1))).is_ok());
    assert!(Grid::new(1, 1440, Some((0, span))).is_err());
    let g = Grid::new(60, 1440, Some((0, 299))).expect("occupancy's");
    assert_eq!((g.step(), g.size(), g.groups()), (60, 1440, Some((0, 299))));
}

#[test]
fn drawn_intervals_equal_expand_and_count_dense_and_sparse() {
    let mut rng = Rng::new(0x4_5);
    for round in 0..200 {
        let size = 1 + rng.next_u64() % 50;
        let step = 1 + (rng.next_u64() % 7) as i64;
        let n = (rng.next_u64() % 60) as usize;
        let rows: Vec<(i32, i64, i64)> = (0..n)
            .map(|_| {
                let g = (rng.next_u64() % 9) as i32 - 4;
                let s = (rng.next_u64() % 2_000) as i64 - 1_000;
                let l = (rng.next_u64() % (3 * size + 3)) as i64 - 2;
                (g, s, l)
            })
            .collect();
        let sparse = Grid::new(step, size as i64, None).expect("grid");
        let want = expand_and_count(&sparse, &rows);
        let (got, _) = run(&sparse, &some(&rows));
        assert_eq!(got, want, "round {round}, sparse");
        let dense = Grid::new(step, size as i64, Some((-4, 4))).expect("grid");
        let (got, notes) = run(&dense, &some(&rows));
        assert_eq!(got, want, "round {round}, dense");
        assert!(notes.is_empty());
        let narrow = Grid::new(step, size as i64, Some((-1, 1))).expect("grid");
        let (got, _) = run(&narrow, &some(&rows));
        assert_eq!(got, want, "round {round}, fallen back");
    }
}

/// Occupancy as written, on (zone, pickup µs, dropoff µs): floor each to the minute, clip the
/// difference to 0..=240, add one, expand, key by zone and minute of day, count.
fn occupancy_as_written(trips: &[(i32, i64, i64)]) -> Vec<Cell> {
    let mut counts: BTreeMap<(i32, i32), i64> = BTreeMap::new();
    for &(zone, pu, dropoff) in trips {
        let (a, b) = (pu.div_euclid(60_000_000), dropoff.div_euclid(60_000_000));
        let n = (b - a).clamp(0, 240) + 1;
        for k in 0..n {
            *counts
                .entry((zone, (a + k).rem_euclid(1440) as i32))
                .or_default() += 1;
        }
    }
    counts.into_iter().map(|((g, b), c)| (g, b, c)).collect()
}

#[test]
fn occupancy_through_the_kernel_equals_occupancy_as_written() {
    let mut rng = Rng::new(2024);
    let base = 1_704_067_200_000_000i64; // 2024-01-01T00:00:00
    let trips: Vec<(i32, i64, i64)> = (0..5_000)
        .map(|_| {
            let zone = 1 + (rng.next_u64() % 265) as i32;
            let pu = base + (rng.next_u64() % (31 * 86_400_000_000)) as i64;
            // Mostly short trips, some across midnight, some over four hours, some backwards.
            let dur = match rng.next_u64() % 10 {
                0 => -((rng.next_u64() % 3_600_000_000) as i64),
                1 => (rng.next_u64() % (20 * 3_600_000_000)) as i64,
                _ => (rng.next_u64() % 3_600_000_000) as i64,
            };
            (zone, pu, pu + dur)
        })
        .collect();
    let length: Int64Array = trips
        .iter()
        .map(|&(_, pu, d)| {
            Some((d.div_euclid(60_000_000) - pu.div_euclid(60_000_000)).clamp(0, 240) + 1)
        })
        .collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("zone", DataType::Int32, false),
        Field::new("pickup_us", DataType::Int64, false),
        Field::new("minutes", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(trips.iter().map(|t| t.0).collect::<Int32Array>()),
            Arc::new(trips.iter().map(|t| t.1).collect::<Int64Array>()),
            Arc::new(length),
        ],
    )
    .expect("batch");
    let want = occupancy_as_written(&trips);
    for groups in [None, Some(json!([0, 299]))] {
        let mut args = json!({"group": "zone", "start": "pickup_us", "length": "minutes",
                              "step": 60_000_000, "size": 1440});
        if let Some(g) = groups {
            args["groups"] = g;
        }
        let kernel = IntervalCoverageCount::new(&args).expect("kernel");
        let (out, notes) = kernel.run(&batch).expect("run");
        assert_eq!(cells(&out), want);
        assert!(notes.is_empty());
        // Split in two and merged by addition, the same cells.
        let (a, _) = kernel.run(&batch.slice(0, 1_234)).expect("a");
        let (b, _) = kernel.run(&batch.slice(1_234, 5_000 - 1_234)).expect("b");
        assert_eq!(cells(&merge(&[a, b]).expect("merge")), want);
    }
}

#[test]
fn partials_merge_by_addition_in_key_order() {
    let g = grid(1, 6);
    let (a, _) = run(&g, &some(&[(2, 4, 3), (1, 0, 1)]));
    let (b, _) = run(&g, &some(&[(2, 5, 2), (0, 1, 1)]));
    let to_batch = |cs: &[Cell]| {
        RecordBatch::try_new(
            output_schema(),
            vec![
                Arc::new(cs.iter().map(|c| c.0).collect::<Int32Array>()),
                Arc::new(cs.iter().map(|c| c.1).collect::<Int32Array>()),
                Arc::new(cs.iter().map(|c| c.2).collect::<Int64Array>()),
            ],
        )
        .expect("batch")
    };
    let merged = merge(&[to_batch(&a), to_batch(&b)]).expect("merge");
    assert_eq!(
        cells(&merged),
        vec![(0, 1, 1), (1, 0, 1), (2, 0, 2), (2, 4, 1), (2, 5, 2)]
    );
    assert_eq!(merge(&[]).expect("none").num_rows(), 0);
    let err =
        merge(&[to_batch(&[(0, 0, i64::MAX)]), to_batch(&[(0, 0, 1)])]).expect_err("overflow");
    assert!(err.to_string().contains("overflows Int64"), "{err}");
    let other = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from(vec![1]))],
    )
    .expect("other");
    let err = merge(&[other]).expect_err("schema");
    assert!(err.to_string().contains("partial 0"), "{err}");
}

fn occupancy_args() -> Value {
    json!({"group": "g", "start": "s", "length": "l", "step": 1, "size": 4})
}

fn input(rows: &[(Option<i32>, i64, i64)]) -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("g", DataType::Int32, true),
            Field::new("s", DataType::Int64, false),
            Field::new("l", DataType::Int64, false),
            Field::new("other", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(rows.iter().map(|r| r.0).collect::<Int32Array>()),
            Arc::new(rows.iter().map(|r| r.1).collect::<Int64Array>()),
            Arc::new(rows.iter().map(|r| r.2).collect::<Int64Array>()),
            Arc::new(moruna_kernel::arrow::array::StringArray::from(vec![
                Some("x");
                rows.len()
            ])),
        ],
    )
    .expect("input")
}

#[test]
fn arguments_are_checked_by_name_and_shape() {
    let ok = IntervalCoverageCount::new(&occupancy_args()).expect("ok");
    assert_eq!(ok.grid(), Grid::new(1, 4, None).expect("grid"));
    assert_eq!(
        ok.inputs().map(|(n, _)| n.to_string()),
        ["g".to_string(), "s".into(), "l".into()]
    );
    let refused = |args: Value, words: &str| {
        let err = IntervalCoverageCount::new(&args).expect_err(words);
        let text = err.to_string();
        assert!(text.contains(&format!("moruna.{NAME}")), "{text}");
        assert!(text.contains(words), "{words}: {text}");
    };
    refused(json!([1]), "must be an object");
    refused(Value::Null, "must be an object");
    let with = |key: &str, value: Value| {
        let mut args = occupancy_args();
        args[key] = value;
        args
    };
    let without = |key: &str| {
        let mut args = occupancy_args();
        args.as_object_mut().expect("object").remove(key);
        args
    };
    refused(with("bucket", json!(1)), "unknown argument `bucket`");
    refused(without("group"), "`group` is required");
    refused(without("step"), "`step` is required");
    refused(without("size"), "`size` is required");
    refused(with("start", json!(3)), "`start` must name a column");
    refused(with("length", json!("")), "`length` must name a column");
    refused(with("step", json!("1")), "`step` must be an integer");
    refused(with("step", json!(0)), "at least 1");
    refused(with("size", json!(1.5)), "`size` must be an integer");
    refused(with("size", json!(0)), "`size` must be 1 to");
    refused(with("groups", json!([1])), "`groups` must be [low, high]");
    refused(
        with("groups", json!([1, 2, 3])),
        "`groups` must be [low, high]",
    );
    refused(
        with("groups", json!([0, 1u64 << 40])),
        "`groups` must be [low, high]",
    );
    refused(
        with("groups", json!("0..9")),
        "`groups` must be [low, high]",
    );
    refused(with("groups", json!([3, 1])), "low at most high");
    let null_domain = IntervalCoverageCount::new(&with("groups", Value::Null)).expect("null");
    assert_eq!(null_domain.grid().groups(), None);
    let dense = IntervalCoverageCount::new(&with("groups", json!([0, 9]))).expect("dense");
    assert_eq!(dense.grid().groups(), Some((0, 9)));
    assert_ne!(dense.fingerprint(), ok.fingerprint());
    assert_eq!(
        IntervalCoverageCount::new(&occupancy_args())
            .expect("again")
            .fingerprint(),
        ok.fingerprint()
    );
}

#[test]
fn the_kernel_declares_and_checks_its_schemas() {
    let kernel = IntervalCoverageCount::new(&occupancy_args()).expect("kernel");
    assert!(matches!(kernel.kind(), KernelKind::Stateless));
    let hints = kernel.hints();
    assert_eq!(hints.releases_gil, Some(true));
    assert!(!hints.uses_device_memory);
    let declared = kernel.declared();
    assert!(declared.is_checkable());
    match declared.output {
        Some(SchemaDecl::Exact(columns)) => {
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, OUTPUT_COLUMNS);
        }
        other => panic!("output declared as {other:?}"),
    }
    let schema = input(&[]).schema();
    match kernel
        .output_schema(&SourceSchema::Table(schema))
        .expect("schema")
    {
        SourceSchema::Table(out) => assert_eq!(out, output_schema()),
        other => panic!("{other:?}"),
    }
    let missing = Arc::new(Schema::new(vec![Field::new("g", DataType::Int32, true)]));
    let err = kernel
        .output_schema(&SourceSchema::Table(missing))
        .expect_err("missing");
    assert!(err.to_string().contains("no column `s`"), "{err}");
    let wrong = Arc::new(Schema::new(vec![
        Field::new("g", DataType::Int64, true),
        Field::new("s", DataType::Int64, true),
        Field::new("l", DataType::Int64, true),
    ]));
    let err = kernel
        .output_schema(&SourceSchema::Table(wrong))
        .expect_err("wrong type");
    assert!(err.to_string().contains("`g` must be Int32"), "{err}");
    let tensor = SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, 3],
    };
    assert!(kernel.output_schema(&tensor).is_err());
    assert_eq!(kernel.accepts().kind, moruna_kernel::PayloadKind::Table);
}

#[derive(Default)]
struct Records(Mutex<Vec<(Option<Level>, String)>>);

impl OutputSink for Records {
    fn bytes(&self, _stage: Option<moruna_kernel::StageId>, _stream: Stream, _bytes: &[u8]) {}

    fn record(&self, _stage: Option<moruna_kernel::StageId>, level: Option<Level>, text: &str) {
        self.0
            .lock()
            .expect("records")
            .push((level, text.to_string()));
    }

    fn hold(&self) -> Box<dyn Send> {
        Box::new(())
    }
}

#[test]
fn apply_gives_the_cells_and_logs_its_notes_as_a_warning() {
    let kernel = IntervalCoverageCount::new(&occupancy_args()).expect("kernel");
    let alloc: Arc<dyn moruna_kernel::Allocator> = Arc::new(FakeAllocator::new());
    let mut state = kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc,
        })
        .expect("init");
    let records = Arc::new(Records::default());
    let sink: Arc<dyn OutputSink> = records.clone();
    let _entered = moruna_kernel::output::enter(&sink, 3);
    let out = kernel
        .apply(
            state.as_mut(),
            Payload::table(input(&[(Some(1), 3, 2), (Some(2), 0, 1)])).expect("payload"),
        )
        .expect("apply");
    let Payload::Table(out, _) = out else {
        panic!("a table")
    };
    assert_eq!(cells(&out), vec![(1, 0, 1), (1, 3, 1), (2, 0, 1)]);
    assert!(
        records.0.lock().expect("records").is_empty(),
        "nothing to say"
    );
    let out = kernel
        .apply(
            &mut NoState,
            Payload::table(input(&[(None, 3, 2), (Some(2), 0, 1)])).expect("payload"),
        )
        .expect("apply");
    assert_eq!(out.rows(), 1);
    let logged = records.0.lock().expect("records").clone();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].0, Some(Level::Warning));
    assert!(
        logged[0].1.contains("1 rows skipped for a null"),
        "{}",
        logged[0].1
    );
    // A batch without the kernel's columns is refused.
    let other = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)])),
        vec![Arc::new(Int64Array::from(vec![1]))],
    )
    .expect("other");
    let err = kernel
        .apply(&mut NoState, Payload::table(other).expect("payload"))
        .expect_err("no columns");
    assert!(err.to_string().contains("no column `g`"), "{err}");
    let wrong = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("g", DataType::Int32, false),
            Field::new("s", DataType::Int32, false),
        ])),
        vec![
            Arc::new(Int32Array::from(vec![1])),
            Arc::new(Int32Array::from(vec![1])),
        ],
    )
    .expect("wrong");
    let err = kernel
        .apply(&mut NoState, Payload::table(wrong).expect("payload"))
        .expect_err("wrong type");
    assert!(err.to_string().contains("`s` must be Int64"), "{err}");
}

#[test]
fn names_and_schema() {
    assert_eq!(NAME, "interval_coverage_count");
    assert_eq!(VERSION, 1);
    assert_eq!(OUTPUT_COLUMNS, ["group", "bucket", "count"]);
    let schema = output_schema();
    assert!(schema.fields().iter().all(|f| !f.is_nullable()));
    assert_eq!(schema.field(2).data_type(), &DataType::Int64);
    // A whole column of nulls has no cells and says how many it skipped.
    let g = grid(1, 3);
    let nulls = Int32Array::new_null(4);
    let (out, notes) = coverage(
        &g,
        &nulls,
        &Int64Array::from(vec![0; 4]),
        &Int64Array::from(vec![1; 4]),
    )
    .expect("nulls");
    assert_eq!(out.num_rows(), 0);
    assert_eq!(notes.skipped_nulls, 4);
    assert_eq!(out.column(0).null_count(), 0);
}

fn refused(grid: &Grid, rows: &[(i32, i64, i64)]) -> String {
    let rows = some(rows);
    let group: Int32Array = rows.iter().map(|r| r.0).collect();
    let start: Int64Array = rows.iter().map(|r| r.1).collect();
    let length: Int64Array = rows.iter().map(|r| r.2).collect();
    coverage(grid, &group, &start, &length)
        .expect_err("refused")
        .to_string()
}

#[test]
fn many_groups_on_a_grid_are_swept_with_the_same_cells() {
    // 20,000 groups on a ring of 1,000: their difference rows, 20,000 * 1,001 cells, would not
    // fit, so the sparse table sweeps the intervals' changes instead.
    let size = 1_000i64;
    assert!(20_000 * (size as u64 + 1) > DENSE_CELLS_LIMIT);
    let mut rng = Rng::new(11);
    let many: Vec<(i32, i64, i64)> = (0..20_000)
        .map(|i| {
            (
                i - 10_000,
                (rng.next_u64() % 5_000) as i64 - 2_500,
                (rng.next_u64() % 30) as i64 - 3,
            )
        })
        .collect();
    // Full turns: one group passes some buckets three times, another every bucket three times.
    let mut rows = many.clone();
    rows.push((5, 990, 2 * size + 15));
    rows.push((5, 3, size));
    rows.push((-10_000, -1, 3 * size));
    let g = grid(1, size);
    let (got, notes) = run(&g, &some(&rows));
    assert_eq!(got, expand_and_count(&g, &rows));
    assert!(notes.is_empty());
    // Overflow while sweeping: in the full turns, and in the running sum.
    let mut turns = many.clone();
    turns.extend(std::iter::repeat_n((7, 0, i64::MAX), 1_001));
    assert!(refused(&g, &turns).contains("group 7 overflows Int64"));
    let mut sum = many;
    sum.extend(std::iter::repeat_n(
        (7, 0, 1_000 * (i64::MAX / 1_000)),
        1_000,
    ));
    sum.extend(std::iter::repeat_n((7, 0, 1), 808));
    assert!(refused(&g, &sum).contains("group 7 overflows Int64"));
    // A grid too wide for even one difference row: 2^24 buckets of 3.
    let wide = grid(3, 1 << 24);
    let rows = [
        (1, -7, 5),
        (1, 3 * ((1 << 24) - 2), 4),
        (2, 0, 1),
        (1, 0, 0),
    ];
    let (got, _) = run(&wide, &some(&rows));
    assert_eq!(got, expand_and_count(&wide, &rows));
    assert_eq!(
        got,
        vec![
            (1, 0, 2),
            (1, 1, 2),
            (1, (1 << 24) - 3, 1),
            (1, (1 << 24) - 2, 2),
            (1, (1 << 24) - 1, 2),
            (2, 0, 1)
        ]
    );
}

#[test]
fn the_catalogue_configures_it_by_identity() {
    assert_eq!(identity().to_string(), "moruna.interval_coverage_count@1");
    assert_eq!(icc::CPU.kernel, identity().kernel());
    assert_eq!(icc::CPU.versions, &[1]);
    assert!(catalogue::implementations().iter().any(|i| {
        i.kernel == "moruna.interval_coverage_count" && i.implementation == catalogue::CPU_REFERENCE
    }));
    let params = Params::json(occupancy_args());
    let kernel = catalogue::configure("moruna.interval_coverage_count@1", &params).expect("@1");
    assert!(matches!(kernel.kind(), KernelKind::Stateless));
    assert_eq!(
        kernel.fingerprint(),
        catalogue::fingerprint(&identity(), &params).expect("fingerprint")
    );
    let err = catalogue::configure("moruna.interval_coverage_count@2", &params)
        .err()
        .expect("@2");
    assert!(err.to_string().contains("no kernel"), "{err}");
    let err = icc::configure(2, &params).err().expect("version 2");
    assert!(err.to_string().contains("version 1 alone"), "{err}");
    let mut arrays = params.clone();
    arrays.arrays.insert("weights".into(), input(&[]));
    let err = icc::configure(1, &arrays).err().expect("arrays");
    assert!(err.to_string().contains("no Arrow parameters"), "{err}");
    let err = icc::configure(1, &Params::default())
        .err()
        .expect("no params");
    assert!(err.to_string().contains("must be an object"), "{err}");
}

/// The corpus, `conformance/interval_coverage_count/`, on the CPU reference, case by case, as
/// the conformance runner will run it: configure, plan, every batch, and the output compared bit
/// for bit (schema included) or the refusal at its step.
#[test]
fn the_corpus_passes_on_the_cpu_reference() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("conformance")
        .join(NAME);
    let corpus = read_corpus(&dir).unwrap_or_else(|p| panic!("{}", p.join("\n")));
    assert_eq!(corpus.kernel, "moruna.interval_coverage_count");
    assert!(corpus.cases.len() >= 20);
    for case in &corpus.cases {
        let refused = |at: Step, err: &moruna_kernel::MorunaError| match &case.expect {
            Expect::Error { at: want, contains } if *want == at => {
                if let Some(text) = contains {
                    assert!(
                        err.to_string().contains(text.as_str()),
                        "{}: {err}",
                        case.name
                    );
                }
            }
            other => panic!(
                "{}: refused at {at:?} ({err}), expected {other:?}",
                case.name
            ),
        };
        let id = format!("{}@{}", corpus.kernel, case.version);
        let kernel = match catalogue::configure(&id, &Params::json(case.params.clone())) {
            Ok(kernel) => kernel,
            Err(err) => {
                refused(Step::Configure, &err);
                continue;
            }
        };
        let (schema, batches) = read_source(&dir, &case.inputs[0]).expect("input");
        let out_schema = match kernel.output_schema_of(&[SourceSchema::Table(schema)]) {
            Ok(SourceSchema::Table(out)) => out,
            Ok(other) => panic!("{}: {other:?}", case.name),
            Err(err) => {
                refused(Step::Schema, &err);
                continue;
            }
        };
        let mut got = Vec::new();
        let mut failed = None;
        for batch in batches {
            match kernel.apply(&mut NoState, Payload::table(batch).expect("payload")) {
                Ok(Payload::Table(out, _)) => got.push(out),
                Ok(_) => panic!("{}: a tensor", case.name),
                Err(err) => {
                    failed = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = failed {
            refused(Step::Apply, &err);
            continue;
        }
        let Expect::Output(file) = &case.expect else {
            panic!("{}: expected a refusal, and it ran", case.name);
        };
        let source = moruna_kernels::conformance::Source {
            file: file.clone(),
            slice: None,
        };
        let (want_schema, want) = read_source(&dir, &source).expect("expected");
        assert_eq!(out_schema.fields(), want_schema.fields(), "{}", case.name);
        let concat = |schema, batches: &[RecordBatch]| {
            moruna_kernel::arrow::compute::concat_batches(schema, batches).expect("concat")
        };
        let got = concat(&out_schema, &got);
        let want = concat(&want_schema, &want);
        assert_eq!(
            got.schema().fields(),
            want.schema().fields(),
            "{}",
            case.name
        );
        assert_eq!(cells(&got), cells(&want), "{}", case.name);
    }
}
