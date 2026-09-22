//! Shared helpers: page rounding (e.4, f.3), the local-path rule (d.1, SO-T16) and the
//! errors a source raises.

use amoru_kernel::{AmoruError, RowRange, Split, SplitId};

/// Round `v` down to a multiple of `page`.
pub(crate) fn page_floor(v: u64, page: u64) -> u64 {
    v - v % page
}

/// Round `v` up to a multiple of `page`.
pub(crate) fn page_ceil(v: u64, page: u64) -> u64 {
    v.div_ceil(page) * page
}

/// A `Source` error naming the split (d.14, h).
pub(crate) fn source_err(split: SplitId, msg: impl Into<String>) -> AmoruError {
    AmoruError::Source {
        split,
        msg: msg.into(),
    }
}

/// The smallest value `morsel.max_bytes` may take (preamble section 5: the range is
/// 64 MiB to 2 GiB). A single row at or above this cannot be made to fit any legal morsel
/// maximum, so it is what SO-I9 calls an oversized row.
///
/// SO-I9 counts "one-row payloads above the range's target", and a source is never told the
/// target: the scheduler computes the range from it and passes the range. This constant is
/// the one bound on the target a source can know, so it is the rule used here. Reported as a
/// documentation item.
pub(crate) const MIN_MORSEL_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// True when a payload of `rows` rows and `bytes` bytes is an oversized single row (SO-I9).
pub(crate) fn oversized(rows: u64, bytes: u64) -> bool {
    rows == 1 && bytes >= MIN_MORSEL_MAX_BYTES
}

/// A `Plan` error (d.14, h).
pub(crate) fn plan_err(msg: impl Into<String>) -> AmoruError {
    AmoruError::Plan(msg.into())
}

/// True when `url` names a local file rather than an object in a store: a plain path, or a
/// `file://` URL. Everything with another scheme is an object URL (d.1, SO-T16).
pub(crate) fn is_local(url: &str) -> bool {
    match url.split_once("://") {
        None => true,
        Some((scheme, _)) => scheme.eq_ignore_ascii_case("file"),
    }
}

/// The filesystem path a local URL names; `file://` is stripped.
pub(crate) fn local_path(url: &str) -> std::path::PathBuf {
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("file") => {
            std::path::PathBuf::from(rest)
        }
        _ => std::path::PathBuf::from(url),
    }
}

/// The rows a read covers: the whole split, or the range it was given, checked against the
/// split (SO-I4; a range outside the split is a `Source` error, h).
pub(crate) fn resolve_range(
    split: &Split,
    rows: Option<RowRange>,
) -> amoru_kernel::Result<RowRange> {
    let whole = RowRange {
        start: 0,
        end: split.rows,
    };
    let Some(range) = rows else {
        return Ok(whole);
    };
    if range.start > range.end || range.end > split.rows {
        return Err(source_err(
            split.id,
            format!(
                "row range [{}, {}) is outside the split's {} rows",
                range.start, range.end, split.rows
            ),
        ));
    }
    if !split.sub_splittable && (range.start != 0 || range.end != split.rows) {
        return Err(source_err(
            split.id,
            format!(
                "row range [{}, {}) narrows a split that is not sub splittable",
                range.start, range.end
            ),
        ));
    }
    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(rows: u64, sub: bool) -> Split {
        Split {
            id: 3,
            rows,
            uncompressed_bytes: 0,
            estimated: false,
            column_bytes: Vec::new(),
            null_counts: Vec::new(),
            sub_splittable: sub,
        }
    }

    #[test]
    fn pages_round_outward() {
        assert_eq!(page_floor(0, 4096), 0);
        assert_eq!(page_floor(4097, 4096), 4096);
        assert_eq!(page_ceil(4097, 4096), 8192);
        assert_eq!(page_ceil(8192, 4096), 8192);
    }

    #[test]
    fn local_urls_are_recognised() {
        assert!(is_local("/tmp/a.parquet"));
        assert!(is_local("file:///tmp/a.parquet"));
        assert!(is_local("FILE:///tmp/a.parquet"));
        assert!(!is_local("s3://bucket/a.parquet"));
        assert_eq!(
            local_path("file:///tmp/a"),
            std::path::PathBuf::from("/tmp/a")
        );
        assert_eq!(local_path("/tmp/a"), std::path::PathBuf::from("/tmp/a"));
        assert_eq!(
            local_path("s3://bucket/a"),
            std::path::PathBuf::from("s3://bucket/a")
        );
    }

    #[test]
    fn ranges_are_checked_against_the_split() {
        let s = split(10, true);
        assert_eq!(resolve_range(&s, None).unwrap().end, 10);
        assert_eq!(
            resolve_range(&s, Some(RowRange { start: 2, end: 5 }))
                .unwrap()
                .start,
            2
        );
        let e = resolve_range(&s, Some(RowRange { start: 2, end: 11 })).unwrap_err();
        assert!(matches!(e, AmoruError::Source { split, .. } if split == 3));
        assert!(resolve_range(&s, Some(RowRange { start: 5, end: 2 })).is_err());
        let whole = split(10, false);
        assert!(resolve_range(&whole, Some(RowRange { start: 0, end: 10 })).is_ok());
        assert!(resolve_range(&whole, Some(RowRange { start: 1, end: 10 })).is_err());
        assert!(oversized(1, MIN_MORSEL_MAX_BYTES));
        assert!(!oversized(2, MIN_MORSEL_MAX_BYTES));
        assert!(!oversized(1, MIN_MORSEL_MAX_BYTES - 1));
        assert!(source_err(1, "x").to_string().contains("split 1"));
        assert!(plan_err("y").to_string().contains("y"));
    }
}
