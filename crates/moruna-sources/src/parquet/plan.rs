//! The Parquet plan (e.1), footer reads (f.1) and row-group pruning (f.2).
//!
//! A local footer is parsed with `std::fs` here, not through the reactor. A reactor read needs
//! a `Buffer`, and a `Buffer` can only come from an `Allocator`, which a source built with
//! `ParquetSource::new` does not have: the arena reaches it only through the `&dyn Allocator`
//! that `read` receives. An object's footer has no `std::fs`, so it is read through
//! `read_object` into the arena, which needs the allocator `ParquetSource::with_allocator`
//! was given (MH 4.6, H7); without one an object URL is a `Plan` error saying so.
//! `head_object` and `list_prefix` list and size objects either way.

use std::sync::Arc;

use moruna_kernel::{Allocator, ObjectMetadata, Reactor, Result, SourceSchema, Split, SplitId};
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::statistics::Statistics;

use super::{ParquetSourceConfig, RowFilter, ScalarValue};
use crate::stats::Counters;
use crate::util::{is_local, local_path, plan_err};

/// What reading an object's footer at plan time needs: the reactor and an allocator.
pub(crate) type ObjectReader<'a> = (&'a Arc<dyn Reactor>, &'a Arc<dyn Allocator>);

/// One planned file: its footer, its projection and the Arrow schema it yields.
pub(crate) struct FileMeta {
    pub(crate) url: String,
    pub(crate) path: std::path::PathBuf,
    /// An object in a store rather than a local file: read with `read_object` at `url`, and
    /// `len` is the size `head_object` or `list_prefix` reported at plan time.
    pub(crate) object: bool,
    /// The file's length at plan time.
    pub(crate) len: u64,
    pub(crate) metadata: Arc<ParquetMetaData>,
    /// Leaf column indices of the projection, in schema order.
    pub(crate) projected: Vec<usize>,
    /// The Arrow schema of the projected columns.
    pub(crate) schema: arrow::datatypes::SchemaRef,
}

/// One planned row group.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) file: usize,
    pub(crate) row_group: usize,
}

/// Every file the configuration names, prefixes listed (e.1). Local paths and `file://` URLs
/// are listed with `read_dir` and sized with `std::fs::metadata`; an object URL goes through
/// `ObjectMetadata` (d.1, SO-T16).
pub(crate) fn files(
    cfg: &ParquetSourceConfig,
    meta: &Arc<dyn ObjectMetadata>,
    counters: &Counters,
    objects: Option<ObjectReader<'_>>,
) -> Result<Vec<FileMeta>> {
    if cfg.urls.is_empty() {
        return Err(plan_err("a ParquetSource needs at least one url"));
    }
    let mut urls = Vec::new();
    let mut objects_found: Vec<(String, u64, ParquetMetaData)> = Vec::new();
    for url in &cfg.urls {
        if is_local(url) {
            let path = local_path(url);
            let info = std::fs::metadata(&path)
                .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
            if info.is_dir() {
                let mut found = Vec::new();
                let entries = std::fs::read_dir(&path)
                    .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
                for entry in entries {
                    let entry = entry.map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
                    let child = entry.path();
                    if child.extension().and_then(|e| e.to_str()) == Some("parquet") {
                        found.push(child);
                    }
                }
                found.sort();
                urls.extend(found.into_iter().map(|p| (p.display().to_string(), p)));
            } else {
                urls.push((url.clone(), path));
            }
        } else {
            // An object URL: list the prefix, or size the one object `head_object` names.
            // Reading its footer needs an arena buffer for the reactor to land the bytes in,
            // which a source built without an allocator does not have (`with_allocator`).
            let Some((reactor, alloc)) = objects else {
                let listed = meta.list_prefix(url).wait()?;
                if listed.is_empty() {
                    meta.head_object(url).wait()?;
                }
                return Err(plan_err(format!(
                    "{url}: a Parquet footer over an object store is read into the arena, and \
                     this source was built without one; build it with \
                     ParquetSource::with_allocator"
                )));
            };
            let mut listed: Vec<moruna_kernel::ObjectMeta> = meta
                .list_prefix(url)
                .wait()?
                .into_iter()
                .filter(|object| object.url.ends_with(".parquet"))
                .collect();
            if listed.is_empty() {
                listed.push(meta.head_object(url).wait()?);
            }
            listed.sort_by(|a, b| a.url.cmp(&b.url));
            for object in listed {
                let object_url = object_url(url, &object.url);
                let metadata = object_footer(reactor, alloc, &object_url, object.size)?;
                Counters::add(&counters.footer_reads, 2);
                objects_found.push((object_url, object.size, metadata));
            }
        }
    }
    if urls.is_empty() && objects_found.is_empty() {
        return Err(plan_err("no Parquet file matched the configuration"));
    }

    let mut parsed: Vec<(String, std::path::PathBuf, bool, u64, ParquetMetaData)> = Vec::new();
    for (url, path) in urls {
        let file =
            std::fs::File::open(&path).map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
        let len = file
            .metadata()
            .map(|m| m.len())
            .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
        let metadata = parquet::file::metadata::ParquetMetaDataReader::new()
            .parse_and_finish(&file)
            .map_err(|e| moruna_kernel::MorunaError::Source {
                split: 0,
                msg: format!("{}: the footer does not parse: {e}", path.display()),
            })?;
        // f.1: the footer length is read speculatively and then in full, so two ranged reads
        // per file at most.
        Counters::add(&counters.footer_reads, 2);
        parsed.push((url, path, false, len, metadata));
    }
    for (url, len, metadata) in objects_found {
        parsed.push((
            url.clone(),
            std::path::PathBuf::from(&url),
            true,
            len,
            metadata,
        ));
    }

    let mut out = Vec::new();
    for (url, path, object, len, metadata) in parsed {
        let metadata = Arc::new(metadata);
        let descr = metadata.file_metadata().schema_descr();
        let projected = project(descr, cfg.columns.as_deref(), &path)?;
        let mask = parquet::arrow::ProjectionMask::leaves(descr, projected.iter().copied());
        let schema = parquet::arrow::parquet_to_arrow_schema_by_columns(
            descr,
            mask,
            metadata.file_metadata().key_value_metadata(),
        )
        .map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
        out.push(FileMeta {
            url,
            path,
            object,
            len,
            metadata,
            projected,
            schema: Arc::new(schema),
        });
    }
    check_one_schema(&out)?;
    Ok(out)
}

/// The URL of a listed object. A store lists keys relative to its container, so a key that
/// is not already a URL is put back under the scheme and container `prefix` names.
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

/// Read and decode one object's footer through the reactor (e.1, f.1): the last 64 KiB in one
/// ranged read, which holds the footer of any ordinary file, and the rest of the metadata in a
/// second when it does not.
fn object_footer(
    reactor: &Arc<dyn Reactor>,
    alloc: &Arc<dyn Allocator>,
    url: &str,
    size: u64,
) -> Result<ParquetMetaData> {
    const TAIL: u64 = 64 * 1024;
    let footer_err = |msg: String| moruna_kernel::MorunaError::Source {
        split: 0,
        msg: format!("{url}: {msg}"),
    };
    if size < 12 {
        return Err(footer_err(format!(
            "{size} bytes is too short to be Parquet"
        )));
    }
    let tail_len = size.min(TAIL);
    let tail = read_range(reactor, alloc, url, size - tail_len, tail_len)?;
    let end = &tail[tail.len() - 8..];
    if &end[4..] != b"PAR1" {
        return Err(footer_err(
            "the file does not end in PAR1 (an encrypted footer is not supported)".into(),
        ));
    }
    let metadata_len = u64::from(u32::from_le_bytes([end[0], end[1], end[2], end[3]]));
    if metadata_len + 8 > size {
        return Err(footer_err(format!(
            "the footer claims {metadata_len} bytes of metadata in a {size} byte file"
        )));
    }
    let decoded = if metadata_len + 8 <= tail_len {
        let start = (tail_len - 8 - metadata_len) as usize;
        ParquetMetaDataReader::decode_metadata(&tail[start..tail.len() - 8])
    } else {
        let whole = read_range(reactor, alloc, url, size - 8 - metadata_len, metadata_len)?;
        ParquetMetaDataReader::decode_metadata(&whole)
    };
    decoded.map_err(|e| footer_err(format!("the footer does not parse: {e}")))
}

/// `len` bytes of an object at `offset`, landed in an arena buffer by the reactor.
fn read_range(
    reactor: &Arc<dyn Reactor>,
    alloc: &Arc<dyn Allocator>,
    url: &str,
    offset: u64,
    len: u64,
) -> Result<bytes::Bytes> {
    let length = usize::try_from(len)
        .map_err(|_| plan_err(format!("{url}: {len} bytes do not fit in memory")))?;
    let tier = if alloc.is_pinned() {
        moruna_kernel::Tier::PinnedHost
    } else {
        moruna_kernel::Tier::Host
    };
    let buffer = alloc.alloc(length, tier)?;
    let buffer = reactor.read_object(url, offset, buffer).wait()?;
    Ok(bytes::Bytes::from_owner(buffer).slice(..length))
}

/// The leaf column indices the projection selects, in schema order. A name no file has is a
/// `Plan` error naming the column and the file (h).
fn project(
    descr: &parquet::schema::types::SchemaDescriptor,
    columns: Option<&[String]>,
    path: &std::path::Path,
) -> Result<Vec<usize>> {
    let Some(columns) = columns else {
        return Ok((0..descr.num_columns()).collect());
    };
    let mut out = Vec::new();
    for name in columns {
        let mut found = false;
        for i in 0..descr.num_columns() {
            let column = descr.column(i);
            if column.path().parts().first().map(String::as_str) == Some(name.as_str()) {
                out.push(i);
                found = true;
            }
        }
        if !found {
            return Err(plan_err(format!(
                "{}: the projection names column {name}, which the file does not have",
                path.display()
            )));
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Every file under one source must agree on the projected columns' types (h, SO-T11).
fn check_one_schema(files: &[FileMeta]) -> Result<()> {
    let Some(first) = files.first() else {
        return Ok(());
    };
    for other in files.iter().skip(1) {
        if first.schema.fields().len() != other.schema.fields().len() {
            return Err(plan_err(format!(
                "{} has {} projected columns and {} has {}; one source has one schema",
                first.url,
                first.schema.fields().len(),
                other.url,
                other.schema.fields().len()
            )));
        }
        for (a, b) in first.schema.fields().iter().zip(other.schema.fields()) {
            if a.name() != b.name() || a.data_type() != b.data_type() {
                return Err(plan_err(format!(
                    "{} and {} disagree on column {}: {:?} and {:?}",
                    first.url,
                    other.url,
                    a.name(),
                    a.data_type(),
                    b.data_type()
                )));
            }
        }
    }
    Ok(())
}

/// One split per surviving row group, ids in file order then row-group order (e.1).
pub(crate) fn splits(
    cfg: &ParquetSourceConfig,
    files: &[FileMeta],
    _counters: &Counters,
) -> Result<(Vec<Entry>, Vec<Split>, u64)> {
    let mut entries = Vec::new();
    let mut splits = Vec::new();
    let mut skipped = 0u64;
    let mut next: SplitId = 0;
    for (index, file) in files.iter().enumerate() {
        for (rg, group) in file.metadata.row_groups().iter().enumerate() {
            if prune(cfg, file, group)? {
                skipped += 1;
                continue;
            }
            let mut column_bytes = Vec::with_capacity(file.projected.len());
            let mut null_counts = Vec::with_capacity(file.projected.len());
            for leaf in &file.projected {
                let chunk = group.column(*leaf);
                column_bytes.push(chunk.uncompressed_size().max(0) as u64);
                null_counts.push(chunk.statistics().and_then(|s| s.null_count_opt()));
            }
            splits.push(Split {
                id: next,
                rows: group.num_rows().max(0) as u64,
                uncompressed_bytes: column_bytes.iter().sum(),
                estimated: false,
                column_bytes,
                null_counts,
                sub_splittable: true,
            });
            entries.push(Entry {
                file: index,
                row_group: rg,
            });
            next += 1;
        }
    }
    Ok((entries, splits, skipped))
}

/// True when every `RowFilter` proves no row of the group can match (f.2). A filter on a
/// column without statistics never skips.
fn prune(
    cfg: &ParquetSourceConfig,
    file: &FileMeta,
    group: &parquet::file::metadata::RowGroupMetaData,
) -> Result<bool> {
    for filter in &cfg.filters {
        let descr = file.metadata.file_metadata().schema_descr();
        let Some(leaf) = (0..descr.num_columns()).find(|i| {
            descr.column(*i).path().parts().first().map(String::as_str) == Some(filter.column())
        }) else {
            return Err(plan_err(format!(
                "{}: the filter names column {}, which the file does not have",
                file.url,
                filter.column()
            )));
        };
        let Some(stats) = group.column(leaf).statistics() else {
            continue;
        };
        if excludes(filter, stats) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when the statistics prove the predicate false for every row of the group.
fn excludes(filter: &RowFilter, stats: &Statistics) -> bool {
    let (min, max) = match bounds(stats) {
        Some(pair) => pair,
        None => return false,
    };
    match filter {
        // Nothing in the group is greater than the value when the largest is not.
        RowFilter::Gt(_, value) => {
            compare(&max, value).is_some_and(|o| o != std::cmp::Ordering::Greater)
        }
        // Nothing is less than the value when the smallest is not.
        RowFilter::Lt(_, value) => {
            compare(&min, value).is_some_and(|o| o != std::cmp::Ordering::Less)
        }
        // The value is outside [min, max].
        RowFilter::Eq(_, value) => {
            compare(&max, value).is_some_and(|o| o == std::cmp::Ordering::Less)
                || compare(&min, value).is_some_and(|o| o == std::cmp::Ordering::Greater)
        }
    }
}

/// The minimum and maximum a statistics record carries, as scalars; `None` when it has none.
fn bounds(stats: &Statistics) -> Option<(ScalarValue, ScalarValue)> {
    macro_rules! pair {
        ($s:expr, $wrap:expr) => {
            match ($s.min_opt(), $s.max_opt()) {
                (Some(min), Some(max)) => Some(($wrap(min.clone()), $wrap(max.clone()))),
                _ => None,
            }
        };
    }
    match stats {
        Statistics::Int32(s) => pair!(s, |v: i32| ScalarValue::I64(i64::from(v))),
        Statistics::Int64(s) => pair!(s, ScalarValue::I64),
        Statistics::Float(s) => pair!(s, |v: f32| ScalarValue::F64(f64::from(v))),
        Statistics::Double(s) => pair!(s, ScalarValue::F64),
        Statistics::Boolean(s) => pair!(s, ScalarValue::Bool),
        Statistics::ByteArray(s) => match (s.min_opt(), s.max_opt()) {
            (Some(min), Some(max)) => Some((
                ScalarValue::Str(String::from_utf8_lossy(min.data()).into_owned()),
                ScalarValue::Str(String::from_utf8_lossy(max.data()).into_owned()),
            )),
            _ => None,
        },
        _ => None,
    }
}

/// Compare a bound against a literal; `None` when the two are of different kinds, which never
/// prunes.
fn compare(bound: &ScalarValue, value: &ScalarValue) -> Option<std::cmp::Ordering> {
    match (bound, value) {
        (ScalarValue::I64(a), ScalarValue::I64(b)) => Some(a.cmp(b)),
        (ScalarValue::U64(a), ScalarValue::U64(b)) => Some(a.cmp(b)),
        (ScalarValue::I64(a), ScalarValue::U64(b)) => u64::try_from(*a).ok().map(|a| a.cmp(b)),
        (ScalarValue::U64(a), ScalarValue::I64(b)) => u64::try_from(*b).ok().map(|b| a.cmp(&b)),
        (ScalarValue::F64(a), ScalarValue::F64(b)) => a.partial_cmp(b),
        (ScalarValue::Str(a), ScalarValue::Str(b)) => Some(a.cmp(b)),
        (ScalarValue::Bool(a), ScalarValue::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// The schema a Parquet source declares: the projected columns of the first file, which every
/// other file agrees with (`check_one_schema`).
pub(crate) fn schema(files: &[FileMeta]) -> Result<SourceSchema> {
    let first = files
        .first()
        .ok_or_else(|| plan_err("no Parquet file matched the configuration"))?;
    Ok(SourceSchema::Table(Arc::clone(&first.schema)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_across_kinds_never_prune() {
        assert_eq!(
            compare(&ScalarValue::I64(1), &ScalarValue::I64(2)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare(&ScalarValue::U64(3), &ScalarValue::I64(3)),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(compare(&ScalarValue::I64(-1), &ScalarValue::U64(1)), None);
        assert_eq!(compare(&ScalarValue::U64(1), &ScalarValue::I64(-1)), None);
        assert_eq!(
            compare(&ScalarValue::Str("a".into()), &ScalarValue::Str("b".into())),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare(&ScalarValue::Bool(false), &ScalarValue::Bool(true)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare(&ScalarValue::F64(1.0), &ScalarValue::F64(f64::NAN)),
            None
        );
        assert_eq!(compare(&ScalarValue::F64(1.0), &ScalarValue::I64(1)), None);
    }

    #[test]
    fn a_filter_names_its_column() {
        assert_eq!(RowFilter::Gt("a".into(), ScalarValue::I64(1)).column(), "a");
        assert_eq!(RowFilter::Lt("b".into(), ScalarValue::I64(1)).column(), "b");
        assert_eq!(RowFilter::Eq("c".into(), ScalarValue::I64(1)).column(), "c");
    }
}
