//! The Arrow IPC file plan (e.9): files, footers, blocks and splits.
//!
//! A file is `ARROW1`, the messages, a footer, the footer's length and `ARROW1` again. The footer
//! holds the schema and one block (offset, metadata length, body length) per record batch. The
//! plan reads the footer, and from each block only its message header, whose record batch table
//! says how many rows the block holds and how many nulls each column has: no body is read, and
//! the rows of a split are exact (SO-I2). Footers and headers are read with `std::fs`, as a
//! Parquet or Vortex footer is; bodies are the reactor's.

use std::collections::HashSet;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::arrow::datatypes::{DataType, Schema, SchemaRef};
use moruna_kernel::arrow::ipc::{self, MetadataVersion};
use moruna_kernel::{MorunaError, Result, SourceSchema, Split};

use super::ArrowIpcSourceConfig;
use crate::stats::Counters;
use crate::util::{is_local, local_path, plan_err};

/// The magic that starts and ends a file.
const MAGIC: &[u8; 6] = b"ARROW1";

/// A message's continuation marker, which a stream starts with and a file does not.
const CONTINUATION: [u8; 4] = [0xff; 4];

/// The most a block's message header may be: a record's framing is under 1 MiB in practice
/// (contracts e.7), and a footer that claims more is read as wrong rather than allocated.
const MAX_HEADER: u64 = 16 << 20;

/// One record batch block of a file, as the footer and the block's message header say.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    /// Where the block's message starts.
    pub(crate) offset: u64,
    /// The message's bytes: its marker, its length and its flatbuffer.
    pub(crate) meta_len: u64,
    /// The body's bytes, from the end of the message.
    pub(crate) body_len: u64,
    /// Rows the message declares.
    pub(crate) rows: u64,
    /// Nulls per top-level column of the file, where the header's nodes say.
    pub(crate) nulls: Vec<Option<u64>>,
}

/// One planned file.
pub(crate) struct FileMeta {
    pub(crate) path: PathBuf,
    /// The file's length at plan time.
    pub(crate) len: u64,
    /// The file's own schema, which every file of the source has.
    pub(crate) schema: SchemaRef,
    /// The projected columns' indices in `schema`, in file order.
    pub(crate) projected: Vec<usize>,
    /// What a read yields: `schema` over `projected`.
    pub(crate) out: SchemaRef,
    pub(crate) blocks: Vec<Block>,
}

/// One planned split: which file and which of its blocks, whole.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) file: usize,
    pub(crate) blocks: Range<usize>,
}

/// What `plan` produces.
pub(crate) struct Planned {
    pub(crate) files: Vec<FileMeta>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) splits: Vec<Split>,
    pub(crate) schema: SourceSchema,
}

/// A `Source` error naming the file: what a malformed file is at plan time (SO-I10).
fn file_err(path: &Path, msg: impl std::fmt::Display) -> MorunaError {
    MorunaError::Source {
        split: 0,
        msg: format!("{}: {msg}", path.display()),
    }
}

/// Every file, its footer and blocks, and the splits (e.9).
pub(crate) fn plan(cfg: &ArrowIpcSourceConfig, counters: &Counters) -> Result<Planned> {
    if cfg.split_rows == Some(0) {
        return Err(plan_err(
            "split_rows is 0: a split holds at least one record batch",
        ));
    }
    let located = locate(cfg)?;
    let mut files: Vec<FileMeta> = Vec::with_capacity(located.len());
    let mut entries = Vec::new();
    let mut splits = Vec::new();
    for (path, len) in located {
        let file = plan_file(cfg, &path, len, counters)?;
        if let Some(first) = files.first() {
            check_same_schema(first, &file)?;
        }
        let index = files.len();
        for (blocks, split) in cut(&file, cfg.split_rows) {
            entries.push(Entry {
                file: index,
                blocks,
            });
            splits.push(Split {
                id: splits.len() as moruna_kernel::SplitId,
                ..split
            });
        }
        files.push(file);
    }
    let first = files
        .first()
        .ok_or_else(|| plan_err("no Arrow IPC file matched the configuration"))?;
    let schema = SourceSchema::Table(Arc::clone(&first.out));
    Ok(Planned {
        files,
        entries,
        splits,
        schema,
    })
}

/// `(path, length)` of every file the configuration names, a directory listed for `*.arrow` in
/// name order.
fn locate(cfg: &ArrowIpcSourceConfig) -> Result<Vec<(PathBuf, u64)>> {
    if cfg.urls.is_empty() {
        return Err(plan_err("an ArrowIpcSource needs at least one url"));
    }
    let mut out = Vec::new();
    for url in &cfg.urls {
        if !is_local(url) {
            return Err(plan_err(format!(
                "{url}: the arrow_ipc source reads local files, a path or a file:// URL"
            )));
        }
        let path = local_path(url);
        let info =
            std::fs::metadata(&path).map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
        if !info.is_dir() {
            out.push((path, info.len()));
            continue;
        }
        let mut found = Vec::new();
        let listing =
            std::fs::read_dir(&path).map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
        for entry in listing {
            let entry = entry.map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
            let child = entry.path();
            if child.extension().and_then(|e| e.to_str()) != Some("arrow") {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|e| plan_err(format!("{}: {e}", child.display())))?;
            if meta.is_file() {
                found.push((child, meta.len()));
            }
        }
        if found.is_empty() {
            return Err(plan_err(format!(
                "{}: the directory holds no `*.arrow` file",
                path.display()
            )));
        }
        found.sort();
        out.extend(found);
    }
    Ok(out)
}

/// `len` bytes at `offset`, or a `Source` error naming the file.
fn read_at(file: &std::fs::File, path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| file_err(path, "a read does not fit in memory"))?;
    let mut bytes = vec![0u8; len];
    file.read_exact_at(&mut bytes, offset).map_err(|e| {
        file_err(
            path,
            format!("reading bytes {offset}..{}: {e}", offset + len as u64),
        )
    })?;
    Ok(bytes)
}

/// One file's plan.
fn plan_file(
    cfg: &ArrowIpcSourceConfig,
    path: &Path,
    len: u64,
    counters: &Counters,
) -> Result<FileMeta> {
    let file =
        std::fs::File::open(path).map_err(|e| plan_err(format!("{}: {e}", path.display())))?;
    // The head: a file starts with the magic, a stream with a message. A stream is a different
    // format with no footer to plan from, so it is refused by what it is (e.9).
    let head = read_at(&file, path, 0, len.min(8))?;
    Counters::add(&counters.footer_reads, 1);
    if head.get(..MAGIC.len()) != Some(&MAGIC[..]) {
        return Err(if head.get(..4) == Some(&CONTINUATION[..]) {
            plan_err(format!(
                "{}: this is an Arrow IPC stream, and the arrow_ipc source reads the file format \
                 only (a file starts with ARROW1 and ends with a footer)",
                path.display()
            ))
        } else {
            plan_err(format!(
                "{}: this is not an Arrow IPC file: it does not start with ARROW1",
                path.display()
            ))
        });
    }
    // magic, padding, footer length, magic: the shortest file there can be.
    if len < 8 + 4 + MAGIC.len() as u64 {
        return Err(file_err(
            path,
            format!("{len} bytes is too short for an Arrow IPC file"),
        ));
    }
    let trailer = read_at(&file, path, len - 10, 10)?;
    Counters::add(&counters.footer_reads, 1);
    if trailer[4..] != MAGIC[..] {
        return Err(file_err(
            path,
            format!(
                "the file ends without ARROW1: it is cut short or is not complete ({len} bytes)"
            ),
        ));
    }
    let footer_len = u64::from(u32::from_le_bytes([
        trailer[0], trailer[1], trailer[2], trailer[3],
    ]));
    let Some(footer_start) = (len - 10).checked_sub(footer_len).filter(|s| *s >= 8) else {
        return Err(file_err(
            path,
            format!("the footer claims {footer_len} bytes in a file of {len}"),
        ));
    };
    let footer_bytes = read_at(&file, path, footer_start, footer_len)?;
    Counters::add(&counters.footer_reads, 1);
    let footer = ipc::root_as_footer(&footer_bytes)
        .map_err(|e| file_err(path, format!("the footer does not parse: {e}")))?;
    if !matches!(footer.version(), MetadataVersion::V4 | MetadataVersion::V5) {
        return Err(file_err(
            path,
            format!(
                "the footer is metadata version {:?}, not V4 or V5",
                footer.version()
            ),
        ));
    }
    let schema = footer
        .schema()
        .ok_or_else(|| file_err(path, "the footer has no schema"))?;
    let schema = moruna_kernel::ipc::footer_schema(schema)
        .map_err(|e| file_err(path, format!("the schema in the footer: {e}")))?;
    if footer.dictionaries().is_some_and(|d| !d.is_empty())
        || schema
            .fields()
            .iter()
            .any(|f| has_dictionary(f.data_type()))
    {
        return Err(plan_err(format!(
            "{}: the file has dictionary encoded columns, which the arrow_ipc source does not read",
            path.display()
        )));
    }
    let projected = project(&schema, cfg.columns.as_deref(), path)?;
    let out = if cfg.columns.is_some() {
        Arc::new(
            schema
                .project(&projected)
                .map_err(|e| plan_err(format!("{}: {e}", path.display())))?,
        )
    } else {
        Arc::clone(&schema)
    };

    let widths = node_counts(&schema);
    let mut blocks = Vec::new();
    for (index, block) in footer.recordBatches().into_iter().flatten().enumerate() {
        let at = |msg: String| file_err(path, format!("block {index}: {msg}"));
        let (Ok(offset), Ok(meta_len), Ok(body_len)) = (
            u64::try_from(block.offset()),
            u64::try_from(block.metaDataLength()),
            u64::try_from(block.bodyLength()),
        ) else {
            return Err(at("the footer gives it a negative offset or length".into()));
        };
        let end = offset
            .checked_add(meta_len)
            .and_then(|e| e.checked_add(body_len))
            .ok_or_else(|| at("its offset and lengths overflow".into()))?;
        if offset < 8 || meta_len < 8 || end > footer_start {
            return Err(at(format!(
                "it runs from byte {offset} to byte {end}, outside the {footer_start} bytes \
                 before the footer"
            )));
        }
        if meta_len > MAX_HEADER {
            return Err(at(format!("its message is {meta_len} bytes")));
        }
        let header = read_at(&file, path, offset, meta_len)?;
        let (rows, nulls) = header_of(&header, body_len, &widths).map_err(at)?;
        blocks.push(Block {
            offset,
            meta_len,
            body_len,
            rows,
            nulls,
        });
    }
    Ok(FileMeta {
        path: path.to_path_buf(),
        len,
        schema,
        projected,
        out,
        blocks,
    })
}

/// The rows and the per column null counts a block's message header declares; an `Err` is a
/// sentence about the block.
fn header_of(
    header: &[u8],
    body_len: u64,
    widths: &[usize],
) -> std::result::Result<(u64, Vec<Option<u64>>), String> {
    if header[..4] != CONTINUATION {
        return Err("its message does not start with a continuation marker".into());
    }
    let declared = u64::from(u32::from_le_bytes([
        header[4], header[5], header[6], header[7],
    ]));
    if declared == 0 || 8 + declared > header.len() as u64 {
        return Err(format!(
            "its message declares {declared} bytes and the footer allows {}",
            header.len() - 8
        ));
    }
    let message = ipc::root_as_message(&header[8..8 + declared as usize])
        .map_err(|e| format!("its message does not parse: {e}"))?;
    let batch = match message.header_type() {
        ipc::MessageHeader::RecordBatch => message.header_as_record_batch(),
        other => return Err(format!("it holds a {other:?} message, not a record batch")),
    }
    .ok_or("its record batch message has no record batch")?;
    let rows = u64::try_from(batch.length())
        .map_err(|_| format!("it declares {} rows", batch.length()))?;
    let body = u64::try_from(message.bodyLength()).map_err(|_| {
        format!(
            "its message declares a body of {} bytes",
            message.bodyLength()
        )
    })?;
    if body > body_len {
        return Err(format!(
            "its message declares a body of {body} bytes and the footer gives it {body_len}"
        ));
    }
    // One node per column, then its children's, in schema order. A header whose nodes are not
    // the schema's is read at read time; here it only means the null counts are not known.
    let nodes: Vec<_> = batch.nodes().into_iter().flatten().collect();
    let mut nulls = Vec::with_capacity(widths.len());
    if nodes.len() == widths.iter().sum::<usize>() {
        let mut at = 0;
        for width in widths {
            nulls.push(u64::try_from(nodes[at].null_count()).ok());
            at += width;
        }
    } else {
        nulls.resize(widths.len(), None);
    }
    Ok((rows, nulls))
}

/// The nodes each top-level column takes in a record batch message: itself and its children.
fn node_counts(schema: &Schema) -> Vec<usize> {
    fn nodes(data_type: &DataType) -> usize {
        match data_type {
            DataType::List(f)
            | DataType::LargeList(f)
            | DataType::ListView(f)
            | DataType::LargeListView(f)
            | DataType::FixedSizeList(f, _)
            | DataType::Map(f, _) => 1 + nodes(f.data_type()),
            DataType::Struct(fields) => {
                1 + fields.iter().map(|f| nodes(f.data_type())).sum::<usize>()
            }
            DataType::Union(fields, _) => {
                1 + fields
                    .iter()
                    .map(|(_, f)| nodes(f.data_type()))
                    .sum::<usize>()
            }
            DataType::RunEndEncoded(ends, values) => {
                1 + nodes(ends.data_type()) + nodes(values.data_type())
            }
            _ => 1,
        }
    }
    schema
        .fields()
        .iter()
        .map(|f| nodes(f.data_type()))
        .collect()
}

/// Whether a type is or holds a dictionary encoded one.
fn has_dictionary(data_type: &DataType) -> bool {
    match data_type {
        DataType::Dictionary(..) => true,
        DataType::List(f)
        | DataType::LargeList(f)
        | DataType::ListView(f)
        | DataType::LargeListView(f)
        | DataType::FixedSizeList(f, _)
        | DataType::Map(f, _) => has_dictionary(f.data_type()),
        DataType::Struct(fields) => fields.iter().any(|f| has_dictionary(f.data_type())),
        DataType::Union(fields, _) => fields.iter().any(|(_, f)| has_dictionary(f.data_type())),
        DataType::RunEndEncoded(_, values) => has_dictionary(values.data_type()),
        _ => false,
    }
}

/// The projected columns' indices in file order; a name the file lacks is a `Plan` error naming
/// the column and the file.
fn project(schema: &Schema, columns: Option<&[String]>, path: &Path) -> Result<Vec<usize>> {
    let Some(columns) = columns else {
        return Ok((0..schema.fields().len()).collect());
    };
    let mut wanted = HashSet::new();
    for name in columns {
        let Some(index) = schema.fields().iter().position(|f| f.name() == name) else {
            return Err(plan_err(format!(
                "the projection names column {name:?}, which {} does not have",
                path.display()
            )));
        };
        wanted.insert(index);
    }
    let mut indices: Vec<usize> = wanted.into_iter().collect();
    indices.sort_unstable();
    Ok(indices)
}

/// Every file of one source has one schema (h).
fn check_same_schema(first: &FileMeta, other: &FileMeta) -> Result<()> {
    if first.schema.fields() == other.schema.fields() {
        return Ok(());
    }
    Err(plan_err(format!(
        "{} and {} have different schemas ({} and {}); one source has one schema",
        first.path.display(),
        other.path.display(),
        describe(&first.schema),
        describe(&other.schema),
    )))
}

fn describe(schema: &Schema) -> String {
    let fields: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("{}: {}", f.name(), f.data_type()))
        .collect();
    format!("[{}]", fields.join(", "))
}

/// A file's blocks cut into splits: one per block, or runs of whole blocks that reach
/// `split_rows`. A file with no block is one split of no rows (SO-I7).
fn cut(file: &FileMeta, split_rows: Option<u64>) -> Vec<(Range<usize>, Split)> {
    let target = split_rows.unwrap_or(1).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    let mut rows = 0u64;
    for (i, block) in file.blocks.iter().enumerate() {
        rows += block.rows;
        if split_rows.is_none() || rows >= target || i + 1 == file.blocks.len() {
            out.push(start..i + 1);
            start = i + 1;
            rows = 0;
        }
    }
    if out.is_empty() {
        out.push(0..0);
    }
    out.into_iter()
        .map(|blocks| {
            let split = split_of(file, &blocks);
            (blocks, split)
        })
        .collect()
}

/// A split over `blocks`, from the headers alone. The bytes are the blocks' lengths, which
/// include the padding that aligns them, so they are an estimate; so is a column's share of
/// them, which the headers do not give. The null counts are the headers' own.
fn split_of(file: &FileMeta, blocks: &Range<usize>) -> Split {
    let chosen = &file.blocks[blocks.clone()];
    let rows = chosen.iter().map(|b| b.rows).sum();
    let bytes: u64 = chosen.iter().map(|b| b.meta_len + b.body_len).sum();
    let columns = file.projected.len().max(1) as u64;
    let column_bytes = file
        .projected
        .iter()
        .enumerate()
        .map(|(i, _)| bytes / columns + u64::from((i as u64) < bytes % columns))
        .collect();
    let null_counts = file
        .projected
        .iter()
        .map(|column| {
            chosen
                .iter()
                .map(|b| b.nulls.get(*column).copied().flatten())
                .try_fold(0u64, |sum, n| n.map(|n| sum + n))
        })
        .collect();
    Split {
        id: 0,
        rows,
        uncompressed_bytes: bytes,
        estimated: true,
        column_bytes,
        null_counts,
        sub_splittable: true,
    }
}
