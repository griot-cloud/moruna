//! `moruna.interval_coverage_count@1` (system design 4B): for each group and each bucket of a
//! grid, how many intervals cover it. Its specification is `spec/interval_coverage_count.md`.
//!
//! The grid is a ring of `size` buckets, each `step` units of `start` wide, from 0: a position
//! `p` is in bucket `floor(p / step) mod size`, so a grid of a day's minutes wraps at midnight.
//! An interval covers `length` consecutive buckets from the bucket of its `start`, wrapping from
//! the last bucket to the first, and counts once in a bucket each time it passes it: the count
//! is always what expanding each interval into its buckets and counting them would give.
//!
//! This is the CPU reference, which defines the right answer. It adds one at each interval's
//! first bucket and subtracts one past its last, then takes a running sum per group, so it never
//! expands an interval and its working set is one difference row per group, whatever the
//! intervals' lengths. A declared group domain gives a dense table over the domain; without one,
//! or for a batch with a group outside it, the groups are kept in a sparse table: difference rows
//! for the groups present, or, when those would not fit [`DENSE_CELLS_LIMIT`], the intervals'
//! changes sorted and summed in order (a sweep). Every table gives the same cells. The kernel is stateless: each call gives its batch's cells, and partial
//! outputs (of several batches, or of several instances) merge by addition ([`merge`]).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, Int32Array, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Int64Type, Schema, SchemaRef};
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Level, MorunaError,
    NoState, Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, SourceSchema, TierPref,
};
use serde_json::Value;

use crate::catalogue::{CPU_REFERENCE, Implementation, KernelId, MORUNA, Params, fingerprint};

/// The kernel's name in the `moruna` namespace.
pub const NAME: &str = "interval_coverage_count";

/// The version this module implements.
pub const VERSION: u32 = 1;

/// The output's columns, in order.
pub const OUTPUT_COLUMNS: [&str; 3] = ["group", "bucket", "count"];

/// The most cells a declared domain's dense table may have, `(high - low + 1) * (size + 1)`:
/// 2^24 Int64 cells, 128 MiB. A larger domain is refused at configure.
pub const DENSE_CELLS_LIMIT: u64 = 1 << 24;

/// The CPU reference in the catalogue (`catalogue::IMPLEMENTATIONS`).
pub const CPU: Implementation = Implementation {
    kernel: "moruna.interval_coverage_count",
    implementation: CPU_REFERENCE,
    versions: &[VERSION],
    configure,
};

/// The kernel's identity, `moruna.interval_coverage_count@1`.
pub fn identity() -> KernelId {
    KernelId {
        namespace: MORUNA.to_string(),
        name: NAME.to_string(),
        version: VERSION,
    }
}

/// Configure `moruna.interval_coverage_count@<version>` (the catalogue's `Configure`): version 1
/// alone, with JSON parameters ([`IntervalCoverageCount::new`]) and no Arrow ones.
pub fn configure(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    if version != VERSION {
        return Err(bad(format!("has version {VERSION} alone, not {version}")));
    }
    if let Some(name) = params.arrays.keys().next() {
        return Err(bad(format!(
            "takes no Arrow parameters, and is given `{name}`"
        )));
    }
    Ok(Arc::new(IntervalCoverageCount::new(&params.json)?))
}

/// A `Plan` error naming the kernel.
fn bad(msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.{NAME}@{VERSION}: {msg}"))
}

/// The output's schema: `group` Int32, `bucket` Int32, `count` Int64, none nullable.
pub fn output_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(OUTPUT_COLUMNS[0], DataType::Int32, false),
        Field::new(OUTPUT_COLUMNS[1], DataType::Int32, false),
        Field::new(OUTPUT_COLUMNS[2], DataType::Int64, false),
    ]))
}

/// The grid and the declared group domain: everything the kernel is configured with besides
/// the names of its input columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grid {
    step: i64,
    size: i32,
    groups: Option<(i32, i32)>,
}

impl Grid {
    /// A grid, checked: `step` at least 1; `size` 1 to `i32::MAX`; a domain `[low, high]` with
    /// `low <= high` whose dense table has at most [`DENSE_CELLS_LIMIT`] cells.
    pub fn new(step: i64, size: i64, groups: Option<(i32, i32)>) -> Result<Grid> {
        if step < 1 {
            return Err(bad(format!("`step` must be at least 1, not {step}")));
        }
        let size = i32::try_from(size)
            .ok()
            .filter(|s| *s >= 1)
            .ok_or_else(|| bad(format!("`size` must be 1 to {}, not {size}", i32::MAX)))?;
        if let Some((low, high)) = groups {
            if low > high {
                return Err(bad(format!(
                    "`groups` must be [low, high] with low at most high, not [{low}, {high}]"
                )));
            }
            let cells = (i64::from(high) - i64::from(low) + 1) as u64 * (size as u64 + 1);
            if cells > DENSE_CELLS_LIMIT {
                return Err(bad(format!(
                    "the domain [{low}, {high}] on a grid of {size} needs a dense table of \
                     {cells} cells, more than {DENSE_CELLS_LIMIT}; declare a smaller domain, or \
                     none for the sparse table"
                )));
            }
        }
        Ok(Grid { step, size, groups })
    }

    /// How many units of `start` one bucket is wide.
    pub fn step(&self) -> i64 {
        self.step
    }

    /// How many buckets the ring has.
    pub fn size(&self) -> i32 {
        self.size
    }

    /// The declared group domain, inclusive at both ends, when there is one.
    pub fn groups(&self) -> Option<(i32, i32)> {
        self.groups
    }

    /// The bucket of position `p`: `floor(p / step) mod size`, in `0..size`.
    pub fn bucket(&self, p: i64) -> i32 {
        // `div_euclid` by a positive step is the floor; `rem_euclid` by a positive size is in
        // `0..size`, which fits an i32 because the size does.
        p.div_euclid(self.step).rem_euclid(i64::from(self.size)) as i32
    }
}

/// What one call has to say besides its cells (decisions of 2026-11-18): never silent, never an
/// error.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Notes {
    /// Rows skipped for a null in group, start or length.
    pub skipped_nulls: u64,
    /// Rows whose group is outside the declared domain; when non-zero, the batch was counted on
    /// the sparse table.
    pub outside_domain: u64,
    /// The first such row, and its group.
    pub first_outside: Option<(usize, i32)>,
}

impl Notes {
    /// True when there is nothing to say.
    pub fn is_empty(&self) -> bool {
        self.skipped_nulls == 0 && self.outside_domain == 0
    }

    /// The note as one line, for the stage's log record; `None` when there is nothing to say.
    pub fn text(&self, grid: &Grid) -> Option<String> {
        let mut parts = Vec::new();
        if self.skipped_nulls > 0 {
            parts.push(format!(
                "{} rows skipped for a null in group, start or length",
                self.skipped_nulls
            ));
        }
        if let (Some((row, group)), Some((low, high))) = (self.first_outside, grid.groups) {
            parts.push(format!(
                "the batch was counted on the sparse table: {} rows have a group outside the \
                 declared domain [{low}, {high}], the first row {row} with group {group}",
                self.outside_domain
            ));
        }
        (!parts.is_empty()).then(|| format!("moruna.{NAME}@{VERSION}: {}", parts.join("; ")))
    }
}

/// An Int32 group's hash: one multiply, enough for a table of groups.
#[derive(Default)]
struct GroupHasher(u64);

impl Hasher for GroupHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(*b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    fn write_i32(&mut self, i: i32) {
        self.0 = u64::from(i as u32).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

fn overflow(group: i32) -> MorunaError {
    bad(format!("a count in group {group} overflows Int64"))
}

/// One interval of `length` buckets (at least 1) from bucket `first`, on a ring of `size`: the
/// full turns it makes, and the buckets of its last partial turn as at most two runs
/// `[from, to)` (two when it wraps).
fn split(size: usize, first: i32, length: i64) -> (i64, [(usize, usize); 2]) {
    let (turns, rem) = (length / size as i64, (length % size as i64) as usize);
    let first = first as usize;
    let end = first + rem;
    if rem == 0 {
        (turns, [(0, 0), (0, 0)])
    } else if end <= size {
        (turns, [(first, end), (0, 0)])
    } else {
        (turns, [(first, size), (0, end - size)])
    }
}

/// Difference rows: per slot, `size + 1` differences and a count of full turns of the ring.
struct Rows {
    size: usize,
    diff: Vec<i64>,
    turns: Vec<i64>,
}

impl Rows {
    fn new(slots: usize, size: i32) -> Rows {
        let size = size as usize;
        Rows {
            size,
            diff: vec![0; slots * (size + 1)],
            turns: vec![0; slots],
        }
    }

    /// Count one interval of `length` buckets (at least 1) from bucket `first` in `slot`.
    fn add(&mut self, slot: usize, group: i32, first: i32, length: i64) -> Result<()> {
        let (turns, runs) = split(self.size, first, length);
        if turns > 0 {
            let t = &mut self.turns[slot];
            *t = t.checked_add(turns).ok_or_else(|| overflow(group))?;
        }
        let row = &mut self.diff[slot * (self.size + 1)..(slot + 1) * (self.size + 1)];
        for (from, to) in runs {
            if from < to {
                row[from] += 1;
                row[to] -= 1;
            }
        }
        Ok(())
    }

    /// The running sum of each `(group, slot)`, in the order given, its non-zero cells.
    fn finish(self, order: impl Iterator<Item = (i32, usize)>) -> Result<RecordBatch> {
        let width = self.size + 1;
        let mut out = Cells::default();
        for (group, slot) in order {
            let row = &self.diff[slot * width..(slot + 1) * width];
            let mut running = self.turns[slot];
            for (bucket, delta) in row[..self.size].iter().enumerate() {
                running = running.checked_add(*delta).ok_or_else(|| overflow(group))?;
                if running != 0 {
                    out.push(group, bucket as i32, running);
                }
            }
        }
        out.batch()
    }
}

/// The output's columns as they are built.
#[derive(Default)]
struct Cells {
    group: Vec<i32>,
    bucket: Vec<i32>,
    count: Vec<i64>,
}

impl Cells {
    fn push(&mut self, group: i32, bucket: i32, count: i64) {
        self.group.push(group);
        self.bucket.push(bucket);
        self.count.push(count);
    }

    /// `count` in every bucket of `[from, to)` of `group`, when it is not zero.
    fn run(&mut self, group: i32, from: usize, to: usize, count: i64) {
        if count != 0 {
            for bucket in from..to {
                self.push(group, bucket as i32, count);
            }
        }
    }

    fn batch(self) -> Result<RecordBatch> {
        batch(self.group, self.bucket, self.count)
    }
}

/// The sparse table's sweep, for groups whose difference rows would not fit
/// [`DENSE_CELLS_LIMIT`]: each interval's changes as events, sorted by group and bucket, then the
/// running sum along them. Its working set is the events, two or four per interval, whatever the
/// grid's size.
fn sweep(
    size: i32,
    groups: &[i32],
    rows: impl Iterator<Item = (usize, i32, i64)>,
) -> Result<RecordBatch> {
    let size = size as usize;
    // Each slot's rank in key order, which the events carry in their upper half.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_unstable_by_key(|&s| groups[s]);
    let mut rank = vec![0u64; groups.len()];
    for (r, &slot) in order.iter().enumerate() {
        rank[slot] = r as u64;
    }
    let mut turns = vec![0i64; groups.len()];
    let mut events: Vec<(u64, i64)> = Vec::new();
    for (slot, first, length) in rows {
        let (full, runs) = split(size, first, length);
        if full > 0 {
            turns[slot] = turns[slot]
                .checked_add(full)
                .ok_or_else(|| overflow(groups[slot]))?;
        }
        for (from, to) in runs {
            if from < to {
                events.push(((rank[slot] << 32) | from as u64, 1));
                if to < size {
                    events.push(((rank[slot] << 32) | to as u64, -1));
                }
            }
        }
    }
    events.sort_unstable_by_key(|&(key, _)| key);
    let mut out = Cells::default();
    let mut at = 0;
    for (r, &slot) in order.iter().enumerate() {
        let group = groups[slot];
        let mut running = turns[slot];
        let mut from = 0usize;
        while at < events.len() && (events[at].0 >> 32) as usize == r {
            let bucket = (events[at].0 & 0xffff_ffff) as usize;
            out.run(group, from, bucket, running);
            while at < events.len() && events[at].0 == ((r as u64) << 32) | bucket as u64 {
                running = running
                    .checked_add(events[at].1)
                    .ok_or_else(|| overflow(group))?;
                at += 1;
            }
            from = bucket;
        }
        out.run(group, from, size, running);
    }
    out.batch()
}

fn batch(group: Vec<i32>, bucket: Vec<i32>, count: Vec<i64>) -> Result<RecordBatch> {
    RecordBatch::try_new(
        output_schema(),
        vec![
            Arc::new(Int32Array::from(group)) as ArrayRef,
            Arc::new(Int32Array::from(bucket)),
            Arc::new(Int64Array::from(count)),
        ],
    )
    .map_err(|e| bad(format!("building the output: {e}")))
}

/// The reference on three arrays of one length: each row's interval counted in its group, and
/// the non-zero cells in key order (group, then bucket), with the call's [`Notes`]. A row with a
/// null in any of the three is skipped and counted in the notes; a row whose length is zero or
/// less covers nothing. A batch with a group outside the declared domain is counted on the
/// sparse table, with the same cells, and the notes say so.
pub fn coverage(
    grid: &Grid,
    group: &Int32Array,
    start: &Int64Array,
    length: &Int64Array,
) -> Result<(RecordBatch, Notes)> {
    let n = group.len();
    if start.len() != n || length.len() != n {
        return Err(bad(format!(
            "the three inputs must be one length, not {n}, {} and {}",
            start.len(),
            length.len()
        )));
    }
    let any_null = group.null_count() + start.null_count() + length.null_count() > 0;
    let valid =
        |i: usize| !any_null || (group.is_valid(i) && start.is_valid(i) && length.is_valid(i));
    let (g, s, l) = (group.values(), start.values(), length.values());
    let mut notes = Notes::default();
    for (i, &gi) in g.iter().enumerate() {
        if !valid(i) {
            notes.skipped_nulls += 1;
        } else if let Some((low, high)) = grid.groups
            && (gi < low || gi > high)
        {
            notes.outside_domain += 1;
            notes.first_outside.get_or_insert((i, gi));
        }
    }
    // The rows that count, as (group, start, length): valid, and of a positive length.
    let counted = move || {
        g.iter()
            .zip(s.iter())
            .zip(l.iter())
            .enumerate()
            .filter(move |&(i, (_, &li))| li > 0 && valid(i))
            .map(|(_, ((&gi, &si), &li))| (gi, si, li))
    };
    if let Some((low, high)) = grid.groups
        && notes.outside_domain == 0
    {
        // The dense table over the domain: group g's row is g - low.
        let span = (i64::from(high) - i64::from(low) + 1) as usize;
        let mut rows = Rows::new(span, grid.size);
        for (gi, si, li) in counted() {
            let slot = (i64::from(gi) - i64::from(low)) as usize;
            rows.add(slot, gi, grid.bucket(si), li)?;
        }
        let order = (low..=high).map(|gi| (gi, (i64::from(gi) - i64::from(low)) as usize));
        return Ok((rows.finish(order)?, notes));
    }
    // The sparse table: a slot per group present, in the order first seen, and each counted
    // row's slot.
    let mut index: HashMap<i32, u32, BuildHasherDefault<GroupHasher>> = HashMap::default();
    let mut groups: Vec<i32> = Vec::new();
    let mut slot_of: Vec<u32> = Vec::with_capacity(n);
    for (gi, _, _) in counted() {
        let next = groups.len() as u32;
        let slot = *index.entry(gi).or_insert(next);
        if slot == next {
            groups.push(gi);
        }
        slot_of.push(slot);
    }
    drop(index);
    let rows = counted()
        .zip(slot_of.iter())
        .map(|((_, si, li), &slot)| (slot as usize, grid.bucket(si), li));
    let cells = groups.len() as u64 * (grid.size as u64 + 1);
    let out = if cells <= DENSE_CELLS_LIMIT {
        // Difference rows for the groups present.
        let mut table = Rows::new(groups.len(), grid.size);
        for (slot, first, li) in rows {
            table.add(slot, groups[slot], first, li)?;
        }
        let mut order: Vec<(i32, usize)> = groups.iter().copied().zip(0..).collect();
        order.sort_unstable();
        table.finish(order.into_iter())?
    } else {
        sweep(grid.size, &groups, rows)?
    };
    Ok((out, notes))
}

/// Partial outputs of this kernel merged by addition: every (group, bucket) once, its counts
/// summed, in key order. Each partial must have the output's schema. A sum that overflows Int64
/// is refused.
pub fn merge(partials: &[RecordBatch]) -> Result<RecordBatch> {
    let schema = output_schema();
    let mut cells: Vec<(i32, i32, i64)> = Vec::new();
    for (at, part) in partials.iter().enumerate() {
        if part.schema().fields() != schema.fields() {
            return Err(bad(format!(
                "merge takes the kernel's outputs, and partial {at} has the schema {:?}",
                part.schema()
            )));
        }
        let g = part.column(0).as_primitive::<Int32Type>();
        let b = part.column(1).as_primitive::<Int32Type>();
        let c = part.column(2).as_primitive::<Int64Type>();
        for i in 0..part.num_rows() {
            cells.push((g.value(i), b.value(i), c.value(i)));
        }
    }
    cells.sort_unstable_by_key(|&(g, b, _)| (g, b));
    let mut group = Vec::new();
    let mut bucket = Vec::new();
    let mut count: Vec<i64> = Vec::new();
    for (g, b, c) in cells {
        match count.last_mut() {
            Some(last) if group.last() == Some(&g) && bucket.last() == Some(&b) => {
                *last = last.checked_add(c).ok_or_else(|| {
                    bad(format!(
                        "the merged count of group {g} bucket {b} overflows Int64"
                    ))
                })?;
            }
            _ => {
                group.push(g);
                bucket.push(b);
                count.push(c);
            }
        }
    }
    batch(group, bucket, count)
}

/// `moruna.interval_coverage_count@1`, configured: the names of its three input columns and the
/// grid.
#[derive(Clone, Debug)]
pub struct IntervalCoverageCount {
    group: String,
    start: String,
    length: String,
    grid: Grid,
    fingerprint: Fingerprint,
}

fn integer(args: &Value, key: &str) -> Result<i64> {
    let value = args
        .get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad(format!("`{key}` is required")))?;
    value
        .as_i64()
        .ok_or_else(|| bad(format!("`{key}` must be an integer, not {value}")))
}

fn domain(args: &Value) -> Result<Option<(i32, i32)>> {
    let Some(value) = args.get("groups").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let ends: Option<Vec<i32>> = value.as_array().and_then(|items| {
        items
            .iter()
            .map(|v| v.as_i64().and_then(|i| i32::try_from(i).ok()))
            .collect()
    });
    match ends.as_deref() {
        Some([low, high]) => Ok(Some((*low, *high))),
        _ => Err(bad(format!(
            "`groups` must be [low, high], two Int32 integers, not {value}"
        ))),
    }
}

fn column_name(args: &Value, key: &str) -> Result<String> {
    match args.get(key).filter(|v| !v.is_null()) {
        Some(Value::String(name)) if !name.is_empty() => Ok(name.clone()),
        Some(other) => Err(bad(format!("`{key}` must name a column, not {other}"))),
        None => Err(bad(format!("`{key}` is required"))),
    }
}

impl IntervalCoverageCount {
    /// The arguments it takes.
    pub const ARGUMENTS: [&'static str; 6] = ["group", "start", "length", "step", "size", "groups"];

    /// Configure the kernel from its arguments: `group`, `start` and `length`, the names of its
    /// Int32, Int64 and Int64 input columns; `step` and `size`, the grid; and `groups`, an
    /// optional `[low, high]` domain. An unknown argument, a missing one or one of the wrong
    /// shape is a `Plan` error naming it.
    pub fn new(args: &Value) -> Result<IntervalCoverageCount> {
        let Value::Object(map) = args else {
            return Err(bad(format!("arguments must be an object, not {args}")));
        };
        if let Some(key) = map.keys().find(|k| !Self::ARGUMENTS.contains(&k.as_str())) {
            return Err(bad(format!(
                "unknown argument `{key}` (expected one of {})",
                Self::ARGUMENTS.join(", ")
            )));
        }
        let grid = Grid::new(
            integer(args, "step")?,
            integer(args, "size")?,
            domain(args)?,
        )?;
        let fingerprint = fingerprint(&identity(), &Params::json(args.clone()))?;
        Ok(IntervalCoverageCount {
            group: column_name(args, "group")?,
            start: column_name(args, "start")?,
            length: column_name(args, "length")?,
            grid,
            fingerprint,
        })
    }

    /// The grid this kernel was configured with.
    pub fn grid(&self) -> Grid {
        self.grid
    }

    /// The input columns, by name and type: group, start, length.
    pub fn inputs(&self) -> [(&str, DataType); 3] {
        [
            (self.group.as_str(), DataType::Int32),
            (self.start.as_str(), DataType::Int64),
            (self.length.as_str(), DataType::Int64),
        ]
    }

    /// The kernel on one batch: its cells in key order, and the call's notes.
    pub fn run(&self, batch: &RecordBatch) -> Result<(RecordBatch, Notes)> {
        let mut arrays = Vec::with_capacity(3);
        for (name, ty) in self.inputs() {
            let column = batch
                .column_by_name(name)
                .ok_or_else(|| bad(format!("the input has no column `{name}`")))?;
            if column.data_type() != &ty {
                return Err(bad(format!(
                    "column `{name}` must be {ty}, and is {}",
                    column.data_type()
                )));
            }
            arrays.push(column);
        }
        coverage(
            &self.grid,
            arrays[0].as_primitive::<Int32Type>(),
            arrays[1].as_primitive::<Int64Type>(),
            arrays[2].as_primitive::<Int64Type>(),
        )
    }
}

impl Kernel for IntervalCoverageCount {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: None,
            uses_device_memory: false,
            releases_gil: Some(true),
            preferred_rows: None,
            resume: ResumePolicy::Reinit,
            state_bytes: None,
            measured_line: None,
        }
    }

    fn declared(&self) -> Declared {
        Declared {
            input: Some(SchemaDecl::Subset(
                self.inputs()
                    .into_iter()
                    .map(|(name, ty)| ColumnDecl::new(name, ty))
                    .collect(),
            )),
            output: Some(SchemaDecl::Exact(
                output_schema()
                    .fields()
                    .iter()
                    .map(|f| ColumnDecl::new(f.name().clone(), f.data_type().clone()))
                    .collect(),
            )),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        let SourceSchema::Table(schema) = input else {
            return Err(bad("takes a table, and the input is a tensor"));
        };
        for (name, ty) in self.inputs() {
            let field = schema
                .field_with_name(name)
                .map_err(|_| bad(format!("the input has no column `{name}`")))?;
            if field.data_type() != &ty {
                return Err(bad(format!(
                    "column `{name}` must be {ty}, and is {}",
                    field.data_type()
                )));
            }
        }
        Ok(SourceSchema::Table(output_schema()))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    /// The batch's cells. A call with something to say (rows skipped for a null, a batch counted
    /// on the sparse table) logs it as a warning attributed to the stage.
    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(bad("takes a table, and was given a tensor"));
        };
        let (out, notes) = self.run(batch)?;
        if let Some(text) = notes.text(&self.grid) {
            moruna_kernel::output::log(Level::Warning, &text);
        }
        drop(input);
        Payload::table(out)
    }
}
