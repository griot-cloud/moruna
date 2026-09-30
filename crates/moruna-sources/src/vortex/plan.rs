//! The Vortex plan (e.7): files, footers, the projection, zone maps and splits.
//!
//! A Vortex file's layout tree records, per column, the row ranges of its data segments and a
//! zone map every `zone_len` rows (8,192 by default) holding each zone's minimum, maximum and
//! null count. The plan reads the footer and the zone maps of the projected columns, takes the
//! zone boundaries as the places a split may begin and end, and cuts runs of whole zones of
//! about `split_bytes` uncompressed bytes. So `null_counts` are the zone maps' own figures, the
//! row counts are exact, and a fixed-width column's bytes are exact; a variable-width column's
//! bytes are not in the file at all and are estimated from its segments (`estimated = true`).

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, AsArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, FieldRef, Schema, SchemaRef, UInt64Type};
use moruna_kernel::{Allocator, MorunaError, ObjectMetadata, Reactor, Result, SourceSchema, Split};
use vortex::array::stream::ArrayStreamExt;
use vortex::array::{ArrayRef, VortexSessionExecute};
use vortex::arrow::ArrowSessionExt;
use vortex::buffer::{Alignment, ByteBuffer};
use vortex::file::{Footer, OpenOptionsSessionExt, SegmentSpec};
use vortex::layout::layouts::zoned::Zoned;
use vortex::layout::scan::scan_builder::ScanBuilder;
use vortex::layout::{LayoutChildType, LayoutReaderContext, LayoutRef};

use super::io::{self, Fetched};
use super::{DEFAULT_SPLIT_BYTES, Engine, VortexSourceConfig};
use crate::stats::Counters;
use crate::util::{is_local, local_path, page_floor, plan_err};

/// The tail the first footer read fetches, as for a Parquet footer (f.1).
const FOOTER_TAIL: usize = 64 * 1024;

/// One planned file.
pub(crate) struct FileMeta {
    pub(crate) url: String,
    pub(crate) path: std::path::PathBuf,
    /// An object in a store rather than a local file.
    pub(crate) object: bool,
    /// The file's length at plan time.
    pub(crate) len: u64,
    /// The footer, kept so a read opens the file with no footer I/O.
    pub(crate) footer: Footer,
    /// The projected column names, in file order.
    pub(crate) projected: Vec<String>,
    /// The Arrow schema a read of this file yields.
    pub(crate) schema: SchemaRef,
}

/// One planned split: which file, and the file row it starts at.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) file: usize,
    pub(crate) start: u64,
}

/// What `plan` produces.
pub(crate) struct Planned {
    pub(crate) files: Vec<FileMeta>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) splits: Vec<Split>,
    pub(crate) schema: SourceSchema,
}

/// How plan-time bytes are fetched: a local file with `std::fs`, as a Parquet footer is; an
/// object through the reactor into the arena, which needs the allocator `with_allocator` was
/// given.
enum PlanReader<'a> {
    Local(std::fs::File),
    Object {
        reactor: &'a Arc<dyn Reactor>,
        alloc: &'a Arc<dyn Allocator>,
        url: String,
    },
}

impl PlanReader<'_> {
    async fn fetch(&self, offset: u64, length: usize, alignment: Alignment) -> Fetched {
        match self {
            PlanReader::Local(file) => {
                use std::os::unix::fs::FileExt;
                let mut bytes = vec![0u8; length];
                file.read_exact_at(&mut bytes, offset)
                    .map_err(|e| plan_read_err(offset, length, &e.to_string()))?;
                Ok(ByteBuffer::copy_from_aligned(bytes, alignment))
            }
            PlanReader::Object {
                reactor,
                alloc,
                url,
            } => {
                let tier = if alloc.is_pinned() {
                    moruna_kernel::Tier::PinnedHost
                } else {
                    moruna_kernel::Tier::Host
                };
                let page = alloc.page_bytes() as u64;
                let lo = page_floor(offset, page);
                let end = offset + length as u64;
                let buffer = alloc.alloc(
                    usize::try_from(end - lo)
                        .map_err(|_| plan_err(format!("{url}: {length} bytes do not fit")))?,
                    tier,
                )?;
                let buffer = reactor
                    .read_object(url, lo, buffer)
                    .await
                    .map_err(|e| plan_read_err(offset, length, &e.to_string()))?;
                let arrow = buffer.into_arrow_buffer()?;
                Ok(io::aligned(
                    arrow.slice_with_length((offset - lo) as usize, length),
                    alignment,
                ))
            }
        }
    }
}

fn plan_read_err(offset: u64, length: usize, msg: &str) -> MorunaError {
    MorunaError::Source {
        split: 0,
        msg: format!("reading bytes {offset}..{}: {msg}", offset + length as u64),
    }
}

/// Every file, its footer and zone maps, and the splits (e.7).
pub(crate) fn plan(
    cfg: &VortexSourceConfig,
    engine: &Engine,
    reactor: &Arc<dyn Reactor>,
    meta: &Arc<dyn ObjectMetadata>,
    alloc: Option<&Arc<dyn Allocator>>,
    counters: &Counters,
) -> Result<Planned> {
    let target = cfg.split_bytes.unwrap_or(DEFAULT_SPLIT_BYTES).max(1);
    let located = locate(cfg, meta, alloc.is_some())?;
    let mut files = Vec::with_capacity(located.len());
    let mut entries = Vec::new();
    let mut splits = Vec::new();
    for (url, path, object, len) in located {
        let reader = if object {
            let Some(alloc) = alloc else {
                return Err(plan_err(format!(
                    "{url}: a Vortex footer over an object store is read into the arena, and this \
                     source was built without one; build it with VortexSource::with_allocator"
                )));
            };
            PlanReader::Object {
                reactor,
                alloc,
                url: url.clone(),
            }
        } else {
            PlanReader::Local(
                std::fs::File::open(&path)
                    .map_err(|e| plan_err(format!("{}: {e}", path.display())))?,
            )
        };
        let planned = plan_file(cfg, engine, &reader, &url, len, target, counters)?;
        let index = files.len();
        for (range, split) in planned.splits {
            entries.push(Entry {
                file: index,
                start: range.start,
            });
            splits.push(Split {
                id: splits.len() as moruna_kernel::SplitId,
                ..split
            });
        }
        files.push(FileMeta {
            url,
            path,
            object,
            len,
            footer: planned.footer,
            projected: planned.projected,
            schema: planned.schema,
        });
    }
    check_one_schema(&files)?;
    let first = files
        .first()
        .ok_or_else(|| plan_err("no Vortex file matched the configuration"))?;
    let schema = SourceSchema::Table(Arc::clone(&first.schema));
    Ok(Planned {
        files,
        entries,
        splits,
        schema,
    })
}

/// `(url, path, object, length)` of every file the configuration names, prefixes listed.
fn locate(
    cfg: &VortexSourceConfig,
    meta: &Arc<dyn ObjectMetadata>,
    can_read_objects: bool,
) -> Result<Vec<(String, std::path::PathBuf, bool, u64)>> {
    if cfg.urls.is_empty() {
        return Err(plan_err("a VortexSource needs at least one url"));
    }
    let mut out = Vec::new();
    for url in &cfg.urls {
        if is_local(url) {
            let path = local_path(url);
            let info = std::fs::metadata(&path)
                .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
            if info.is_dir() {
                let mut found = Vec::new();
                let listing = std::fs::read_dir(&path)
                    .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
                for entry in listing {
                    let entry = entry.map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
                    let child = entry.path();
                    if child.extension().and_then(|e| e.to_str()) == Some("vortex") {
                        let len = entry
                            .metadata()
                            .map_err(|e| plan_err(format!("{}: {e}", child.display())))?
                            .len();
                        found.push((child, len));
                    }
                }
                found.sort();
                for (child, len) in found {
                    out.push((child.display().to_string(), child, false, len));
                }
            } else {
                out.push((url.clone(), path, false, info.len()));
            }
        } else {
            let mut listed: Vec<moruna_kernel::ObjectMeta> = meta
                .list_prefix(url)
                .wait()?
                .into_iter()
                .filter(|object| object.url.ends_with(".vortex"))
                .collect();
            if listed.is_empty() {
                listed.push(meta.head_object(url).wait()?);
            }
            if !can_read_objects {
                // Said at the first object, before any of its bytes are asked for.
                return Err(plan_err(format!(
                    "{url}: a Vortex footer over an object store is read into the arena, and this \
                     source was built without one; build it with VortexSource::with_allocator"
                )));
            }
            listed.sort_by(|a, b| a.url.cmp(&b.url));
            for object in listed {
                let object_url = object_url(url, &object.url);
                out.push((
                    object_url.clone(),
                    std::path::PathBuf::from(&object_url),
                    true,
                    object.size,
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(plan_err("no Vortex file matched the configuration"));
    }
    Ok(out)
}

/// The URL of a listed object: a store lists keys relative to its container.
fn object_url(prefix: &str, listed: &str) -> String {
    if listed.contains("://") {
        return listed.to_string();
    }
    let Some((scheme, rest)) = prefix.split_once("://") else {
        return listed.to_string();
    };
    let container = rest.split('/').next().unwrap_or_default();
    format!("{scheme}://{container}/{}", listed.trim_start_matches('/'))
}

/// One file's plan.
struct FilePlan {
    footer: Footer,
    projected: Vec<String>,
    schema: SchemaRef,
    splits: Vec<(Range<u64>, Split)>,
}

/// What the layout tree says about one projected column.
#[derive(Default)]
struct ColumnLayout {
    /// `(rows, bytes)`: every data segment and the file rows it serves.
    leaves: Vec<(Range<u64>, u64)>,
    /// `(rows, zone length, zones child)`: every zone map.
    zoned: Vec<(Range<u64>, u64, LayoutRef)>,
}

fn plan_file(
    cfg: &VortexSourceConfig,
    engine: &Engine,
    reader: &PlanReader<'_>,
    url: &str,
    len: u64,
    target: u64,
    counters: &Counters,
) -> Result<FilePlan> {
    let footer_err = |e: &dyn std::fmt::Display| MorunaError::Source {
        split: 0,
        msg: format!("{url}: the Vortex footer does not parse: {e}"),
    };
    let session = &engine.session;
    // The footer, and the file's natural splits for a file without zone maps.
    let opened = io::drive(
        &engine.runtime,
        url,
        len,
        |source| async move {
            let file = session
                .open_options()
                .with_file_size(len)
                .with_initial_read_size(FOOTER_TAIL)
                .open(source)
                .await?;
            let natural = file.splits()?;
            Ok((file.footer().clone(), natural))
        },
        |offset, length, alignment| {
            Counters::add(&counters.footer_reads, 1);
            reader.fetch(offset, length, alignment)
        },
    );
    let (footer, natural) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => return Err(footer_err(&e)),
        Err(e) => return Err(footer_err(&e)),
    };

    let dtype = footer.dtype().clone();
    let Some(fields) = dtype.as_struct_fields_opt() else {
        return Err(plan_err(format!(
            "{url}: the file holds {dtype}, not a table (a struct of columns)"
        )));
    };
    let names: Vec<String> = fields.names().iter().map(|n| n.to_string()).collect();
    let projected = project(&names, cfg.columns.as_deref(), url)?;
    let full = session
        .arrow()
        .to_arrow_schema(&dtype)
        .map_err(|e| plan_err(format!("{url}: no Arrow schema for {dtype}: {e}")))?;
    let mut out_fields: Vec<FieldRef> = Vec::with_capacity(projected.len());
    for name in &projected {
        let field = full
            .field_with_name(name)
            .map_err(|e| plan_err(format!("{url}: {e}")))?;
        out_fields.push(Arc::new(plain_field(field)));
    }
    let schema: SchemaRef = Arc::new(Schema::new(out_fields));

    // The projected columns' layouts, segment bytes and zone maps.
    let root = footer.layout().clone();
    let segments: Vec<SegmentSpec> = footer.segment_map().iter().cloned().collect();
    let mut columns = Vec::with_capacity(projected.len());
    for name in &projected {
        let mut column = ColumnLayout::default();
        if let Some(layout) = field_layout(&root, name).map_err(|e| footer_err(&e))? {
            walk(&layout, 0, None, &segments, &mut column).map_err(|e| footer_err(&e))?;
        } else {
            // A root that is not split by column: the whole tree serves every column.
            walk(&root, 0, None, &segments, &mut column).map_err(|e| footer_err(&e))?;
        }
        columns.push(column);
    }
    let null_counts = zone_null_counts(engine, reader, &footer, url, len, &columns, counters)?;

    let rows = footer.row_count();
    let boundaries = boundaries(rows, &columns, &natural);
    let intervals: Vec<Range<u64>> = boundaries
        .iter()
        .zip(boundaries.iter().skip(1))
        .map(|(a, b)| *a..*b)
        .collect();
    let per_column: Vec<Vec<(u64, bool)>> = schema
        .fields()
        .iter()
        .zip(&columns)
        .map(|(field, column)| interval_bytes(field, column, &intervals))
        .collect();
    let per_column_nulls: Vec<Vec<Option<u64>>> = null_counts
        .iter()
        .map(|zones| interval_nulls(zones, &intervals))
        .collect();

    let mut splits = Vec::new();
    let mut open: Option<(usize, u64)> = None;
    for i in 0..intervals.len() {
        let (first, acc) = open.unwrap_or((i, 0));
        let acc = acc + per_column.iter().map(|c| c[i].0).sum::<u64>();
        if acc >= target || i + 1 == intervals.len() {
            splits.push(make_split(
                &intervals,
                first..i + 1,
                &per_column,
                &per_column_nulls,
            ));
            open = None;
        } else {
            open = Some((first, acc));
        }
    }
    if splits.is_empty() {
        // An empty file is one split of zero rows (SO-I7).
        splits.push((
            0..0,
            Split {
                id: 0,
                rows: 0,
                uncompressed_bytes: 0,
                estimated: false,
                column_bytes: vec![0; projected.len()],
                null_counts: vec![Some(0); projected.len()],
                sub_splittable: true,
            },
        ));
    }
    Ok(FilePlan {
        footer,
        projected,
        schema,
        splits,
    })
}

/// The projected names in file order; a name the file does not have is a `Plan` error naming
/// the column and the file (h).
fn project(names: &[String], columns: Option<&[String]>, url: &str) -> Result<Vec<String>> {
    let Some(columns) = columns else {
        return Ok(names.to_vec());
    };
    for name in columns {
        if !names.contains(name) {
            return Err(plan_err(format!(
                "{url}: the projection names column {name}, which the file does not have"
            )));
        }
    }
    Ok(names
        .iter()
        .filter(|n| columns.contains(n))
        .cloned()
        .collect())
}

/// The Arrow type a read produces for a Vortex column: the library's preferred type with the
/// view types replaced by their offset forms (`Utf8View` → `Utf8`), which is what the Parquet
/// source produces and what every kernel and sink in this runtime reads.
fn plain_field(field: &Field) -> Field {
    Field::new(
        field.name(),
        plain_type(field.data_type()),
        field.is_nullable(),
    )
    .with_metadata(field.metadata().clone())
}

pub(crate) fn plain_type(data_type: &DataType) -> DataType {
    match data_type {
        DataType::Utf8View => DataType::Utf8,
        DataType::BinaryView => DataType::Binary,
        DataType::ListView(inner) | DataType::List(inner) => {
            DataType::List(Arc::new(plain_field(inner)))
        }
        DataType::LargeListView(inner) | DataType::LargeList(inner) => {
            DataType::LargeList(Arc::new(plain_field(inner)))
        }
        DataType::FixedSizeList(inner, n) => {
            DataType::FixedSizeList(Arc::new(plain_field(inner)), *n)
        }
        DataType::Struct(fields) => {
            DataType::Struct(fields.iter().map(|f| Arc::new(plain_field(f))).collect())
        }
        other => other.clone(),
    }
}

/// The child of a struct root that holds column `name`, when the root is split by column.
fn field_layout(root: &LayoutRef, name: &str) -> vortex::error::VortexResult<Option<LayoutRef>> {
    let children = root.children()?;
    for (child, kind) in children.into_iter().zip(root.child_types()) {
        if let LayoutChildType::Field(field) = kind
            && field.as_ref() == name
        {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// Collect a column's data segments with the file rows they serve, and its zone maps. `aux` is
/// the row range an auxiliary child (a dictionary's values) is spread over: its own rows are
/// not the file's.
fn walk(
    layout: &LayoutRef,
    start: u64,
    aux: Option<Range<u64>>,
    segments: &[SegmentSpec],
    out: &mut ColumnLayout,
) -> vortex::error::VortexResult<()> {
    let rows = aux.clone().unwrap_or(start..start + layout.row_count());
    for id in layout.segment_ids() {
        let bytes = segments
            .get(*id as usize)
            .map_or(0, |s| u64::from(s.length));
        out.leaves.push((rows.clone(), bytes));
    }
    let children = layout.children()?;
    if let Some(zoned) = layout.as_opt::<Zoned>() {
        // The data child is transparent; the zones child is statistics, not payload.
        if let Some(data) = children.first() {
            walk(data, start, aux.clone(), segments, out)?;
        }
        if let (Some(zones), None) = (children.get(1), &aux)
            && zoned.zone_len() > 0
        {
            out.zoned
                .push((rows, zoned.zone_len() as u64, Arc::clone(zones)));
        }
        return Ok(());
    }
    for (child, kind) in children.iter().zip(layout.child_types()) {
        match kind {
            LayoutChildType::Chunk((_, offset)) => match &aux {
                Some(range) => walk(child, start, Some(range.clone()), segments, out)?,
                None => walk(child, start + offset, None, segments, out)?,
            },
            LayoutChildType::Transparent(_) | LayoutChildType::Field(_) => {
                walk(child, start, aux.clone(), segments, out)?
            }
            LayoutChildType::Auxiliary(_) => walk(child, start, Some(rows.clone()), segments, out)?,
        }
    }
    Ok(())
}

/// Every zone of every projected column, `(file rows, null count)`, read from the zone maps
/// through the plan reader (f.8).
#[allow(clippy::type_complexity)]
fn zone_null_counts(
    engine: &Engine,
    reader: &PlanReader<'_>,
    footer: &Footer,
    url: &str,
    len: u64,
    columns: &[ColumnLayout],
    counters: &Counters,
) -> Result<Vec<Vec<(Range<u64>, Option<u64>)>>> {
    let session = &engine.session;
    let wanted: Vec<(usize, Range<u64>, u64, LayoutRef)> = columns
        .iter()
        .enumerate()
        .flat_map(|(i, c)| {
            c.zoned
                .iter()
                .map(move |(rows, zone_len, zones)| (i, rows.clone(), *zone_len, zones.clone()))
        })
        .collect();
    let mut out: Vec<Vec<(Range<u64>, Option<u64>)>> = vec![Vec::new(); columns.len()];
    if wanted.is_empty() {
        return Ok(out);
    }
    let footer = footer.clone();
    let layouts: Vec<LayoutRef> = wanted.iter().map(|w| w.3.clone()).collect();
    let tables = io::drive(
        &engine.runtime,
        url,
        len,
        |source| async move {
            let file = session
                .open_options()
                .with_footer(footer)
                .with_file_size(len)
                .open(source)
                .await?;
            let mut tables: Vec<ArrayRef> = Vec::with_capacity(layouts.len());
            for zones in layouts {
                let reader = zones.new_reader(
                    "zones".into(),
                    file.segment_source(),
                    session,
                    &LayoutReaderContext::default(),
                )?;
                let table = ScanBuilder::new(session.clone(), reader)
                    .into_array_stream()?
                    .read_all()
                    .await?;
                tables.push(table);
            }
            Ok(tables)
        },
        |offset, length, alignment| {
            Counters::add(&counters.footer_reads, 1);
            reader.fetch(offset, length, alignment)
        },
    );
    let zone_err = |e: &dyn std::fmt::Display| MorunaError::Source {
        split: 0,
        msg: format!("{url}: a zone map does not read: {e}"),
    };
    let tables = match tables {
        Ok(Ok(tables)) => tables,
        Ok(Err(e)) => return Err(zone_err(&e)),
        Err(e) => return Err(zone_err(&e)),
    };
    let mut ctx = session.create_execution_ctx();
    for ((column, rows, zone_len, _), table) in wanted.into_iter().zip(tables) {
        let arrow = session
            .arrow()
            .execute_arrow(table, None, &mut ctx)
            .map_err(|e| zone_err(&e))?;
        let counts = arrow.as_struct_opt().and_then(|s| {
            s.fields()
                .iter()
                .position(|f| f.name().contains("null_count"))
                .and_then(|i| {
                    moruna_kernel::arrow::compute::cast(s.column(i), &DataType::UInt64).ok()
                })
        });
        let zones = rows.end.saturating_sub(rows.start).div_ceil(zone_len);
        for zone in 0..zones {
            let lo = rows.start + zone * zone_len;
            let hi = (lo + zone_len).min(rows.end);
            let count = counts.as_ref().and_then(|c| {
                let c = c.as_primitive::<UInt64Type>();
                let i = zone as usize;
                (i < c.len() && c.is_valid(i)).then(|| c.value(i))
            });
            if let Some(slot) = out.get_mut(column) {
                slot.push((lo..hi, count));
            }
        }
    }
    Ok(out)
}

/// Where a split may begin or end: every zone boundary of a projected column, or the file's
/// natural splits when no projected column has a zone map.
fn boundaries(rows: u64, columns: &[ColumnLayout], natural: &[Range<u64>]) -> Vec<u64> {
    let mut set = BTreeSet::from([0, rows]);
    for column in columns {
        for (range, zone_len, _) in &column.zoned {
            let mut at = range.start;
            while at < range.end {
                set.insert(at);
                at += zone_len;
            }
        }
    }
    if set.len() == 2 {
        for range in natural {
            set.insert(range.start.min(rows));
        }
    }
    set.into_iter().filter(|b| *b <= rows).collect()
}

/// A column's bytes over each interval, and whether the figure is an estimate. A fixed-width
/// column's Arrow bytes follow from its rows; any other column's are estimated by spreading its
/// data segments' bytes over the rows they serve, plus its offsets.
fn interval_bytes(
    field: &Field,
    column: &ColumnLayout,
    intervals: &[Range<u64>],
) -> Vec<(u64, bool)> {
    let validity = |rows: u64| {
        if field.is_nullable() {
            rows.div_ceil(8)
        } else {
            0
        }
    };
    if let Some(_width) = fixed_bytes(field.data_type(), 1) {
        return intervals
            .iter()
            .map(|r| {
                let rows = r.end - r.start;
                (
                    fixed_bytes(field.data_type(), rows).unwrap_or(0) + validity(rows),
                    false,
                )
            })
            .collect();
    }
    let mut bytes = vec![0f64; intervals.len()];
    for (rows, leaf_bytes) in &column.leaves {
        let span = (rows.end - rows.start).max(1) as f64;
        let first = intervals.partition_point(|r| r.end <= rows.start);
        for (i, interval) in intervals.iter().enumerate().skip(first) {
            if interval.start >= rows.end {
                break;
            }
            let overlap = interval.end.min(rows.end) - interval.start.max(rows.start);
            bytes[i] += *leaf_bytes as f64 * overlap as f64 / span;
        }
    }
    intervals
        .iter()
        .zip(bytes)
        .map(|(r, b)| {
            let rows = r.end - r.start;
            let offsets = match field.data_type() {
                DataType::Utf8 | DataType::Binary | DataType::List(_) => 4 * (rows + 1),
                DataType::LargeUtf8 | DataType::LargeBinary | DataType::LargeList(_) => {
                    8 * (rows + 1)
                }
                _ => 0,
            };
            (b.round() as u64 + offsets + validity(rows), true)
        })
        .collect()
}

/// The bytes `rows` values of a fixed-width Arrow type occupy; `None` for a variable width.
fn fixed_bytes(data_type: &DataType, rows: u64) -> Option<u64> {
    match data_type {
        DataType::Null => Some(0),
        DataType::Boolean => Some(rows.div_ceil(8)),
        DataType::FixedSizeList(inner, n) => {
            fixed_bytes(inner.data_type(), rows * u64::try_from(*n).ok()?)
        }
        DataType::FixedSizeBinary(n) => Some(rows * u64::try_from(*n).ok()?),
        other => other.primitive_width().map(|w| rows * w as u64),
    }
}

/// A column's null count over each interval: the zone's own figure when the interval is one
/// whole zone of this column, `None` when the column has no zone map there.
fn interval_nulls(
    zones: &[(Range<u64>, Option<u64>)],
    intervals: &[Range<u64>],
) -> Vec<Option<u64>> {
    intervals
        .iter()
        .map(|interval| {
            if interval.start == interval.end {
                return Some(0);
            }
            zones
                .iter()
                .find(|(zone, _)| *zone == *interval)
                .and_then(|(_, count)| *count)
        })
        .collect()
}

/// The split over intervals `run`.
fn make_split(
    intervals: &[Range<u64>],
    run: Range<usize>,
    per_column: &[Vec<(u64, bool)>],
    per_column_nulls: &[Vec<Option<u64>>],
) -> (Range<u64>, Split) {
    let start = intervals.get(run.start).map_or(0, |r| r.start);
    let end = intervals
        .get(run.end.saturating_sub(1))
        .map_or(start, |r| r.end);
    let mut column_bytes = Vec::with_capacity(per_column.len());
    let mut estimated = false;
    for column in per_column {
        let mut sum = 0;
        for (bytes, guess) in &column[run.clone()] {
            sum += bytes;
            estimated |= guess;
        }
        column_bytes.push(sum);
    }
    let null_counts = per_column_nulls
        .iter()
        .map(|column| {
            column[run.clone()]
                .iter()
                .try_fold(0u64, |acc, n| n.map(|n| acc + n))
        })
        .collect();
    (
        start..end,
        Split {
            id: 0,
            rows: end - start,
            uncompressed_bytes: column_bytes.iter().sum(),
            estimated,
            column_bytes,
            null_counts,
            sub_splittable: true,
        },
    )
}

/// Every file under one source must agree on the projected columns' types (h, SO-T11).
fn check_one_schema(files: &[FileMeta]) -> Result<()> {
    let Some(first) = files.first() else {
        return Ok(());
    };
    for other in files.iter().skip(1) {
        if first.schema.fields() != other.schema.fields() {
            let named = first
                .schema
                .fields()
                .iter()
                .zip(other.schema.fields())
                .find(|(a, b)| a != b)
                .map(|(a, _)| a.name().clone())
                .unwrap_or_else(|| "count".to_string());
            return Err(plan_err(format!(
                "{} and {} disagree on column {named}; one source has one schema",
                first.url, other.url
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_types_become_offset_types() {
        assert_eq!(plain_type(&DataType::Utf8View), DataType::Utf8);
        assert_eq!(plain_type(&DataType::BinaryView), DataType::Binary);
        let item = Arc::new(Field::new("item", DataType::Utf8View, true));
        assert_eq!(
            plain_type(&DataType::ListView(item.clone())),
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        );
        assert_eq!(
            plain_type(&DataType::LargeListView(item.clone())),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Utf8, true)))
        );
        assert!(matches!(
            plain_type(&DataType::FixedSizeList(item.clone(), 3)),
            DataType::FixedSizeList(_, 3)
        ));
        assert!(matches!(
            plain_type(&DataType::Struct(vec![item].into())),
            DataType::Struct(_)
        ));
        assert_eq!(plain_type(&DataType::Int8), DataType::Int8);
    }

    #[test]
    fn fixed_widths() {
        assert_eq!(fixed_bytes(&DataType::Int64, 10), Some(80));
        assert_eq!(fixed_bytes(&DataType::Boolean, 10), Some(2));
        assert_eq!(fixed_bytes(&DataType::Null, 10), Some(0));
        assert_eq!(fixed_bytes(&DataType::FixedSizeBinary(4), 10), Some(40));
        let f32s = Arc::new(Field::new("item", DataType::Float32, false));
        assert_eq!(
            fixed_bytes(&DataType::FixedSizeList(f32s, 3), 10),
            Some(120)
        );
        assert_eq!(fixed_bytes(&DataType::Utf8, 10), None);
    }

    #[test]
    fn variable_widths_are_spread_over_their_segments() {
        let column = ColumnLayout {
            leaves: vec![(0..100, 1000), (100..200, 500)],
            zoned: Vec::new(),
        };
        let field = Field::new("s", DataType::Utf8, true);
        let out = interval_bytes(&field, &column, &[0..50, 50..150, 150..200]);
        assert_eq!(out[0], (500 + 4 * 51 + 7, true));
        assert_eq!(out[1], (500 + 250 + 4 * 101 + 13, true));
        assert_eq!(out[2], (250 + 4 * 51 + 7, true));
        let large = Field::new("b", DataType::LargeBinary, false);
        assert_eq!(
            interval_bytes(&large, &column, std::slice::from_ref(&(0..10)))[0],
            (100 + 88, true)
        );
        let structs = Field::new("x", DataType::Struct(Vec::<Field>::new().into()), false);
        assert_eq!(
            interval_bytes(&structs, &column, std::slice::from_ref(&(0..10)))[0],
            (100, true)
        );
    }

    #[test]
    fn nulls_come_from_whole_zones_only() {
        let zones = vec![(0..8, Some(2)), (8..16, None), (16..20, Some(1))];
        assert_eq!(
            interval_nulls(&zones, &[0..8, 8..16, 16..20, 20..20, 0..4]),
            vec![Some(2), None, Some(1), Some(0), None]
        );
    }

    #[test]
    fn boundaries_fall_back_to_natural_splits() {
        let none = ColumnLayout::default();
        assert_eq!(
            boundaries(30, std::slice::from_ref(&none), &[0..10, 10..30]),
            vec![0, 10, 30]
        );
    }

    #[test]
    fn projection_keeps_file_order_and_names_what_is_missing() {
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            project(&names, Some(&["c".to_string(), "a".to_string()]), "f").unwrap(),
            vec!["a".to_string(), "c".to_string()]
        );
        let e = project(&names, Some(&["z".to_string()]), "f.vortex").unwrap_err();
        assert!(e.to_string().contains("column z") && e.to_string().contains("f.vortex"));
        assert_eq!(project(&names, None, "f").unwrap(), names);
    }

    #[test]
    fn listed_objects_keep_their_container() {
        assert_eq!(object_url("s3://b/p/", "p/a.vortex"), "s3://b/p/a.vortex");
        assert_eq!(
            object_url("s3://b/p/", "gs://x/a.vortex"),
            "gs://x/a.vortex"
        );
        assert_eq!(object_url("plain", "a.vortex"), "a.vortex");
    }
}
