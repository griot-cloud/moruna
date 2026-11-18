//! An object's range read into a buffer the caller already holds (system design 3A, item 3.5b).
//!
//! The caller has charged the buffer before the request goes out: in the engine it is the arena
//! buffer the source's drive admitted the read for. The body comes off the connection a piece at
//! a time, and each piece is copied into place as it arrives, so the read holds outside the
//! buffer no more than the piece on its way in (the connector's chunk: 64 KiB on a node's
//! `moruna-net` connector, a frame of the HTTP client's elsewhere), never the range. A file the
//! local store hands over is read straight into the buffer.
//!
//! `object_store`'s `get_range` collects the whole body into one `Bytes` first, so for the length
//! of a read its range was held twice, once in the buffer and once outside it, where nothing
//! charged it (RE-T23).

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use object_store::path::Path;
use object_store::{GetOptions, GetRange, GetResultPayload, ObjectStore};

/// Read `range` of `location` into `dst`, which holds exactly the range's bytes, and give `dst`
/// back with the outcome. The store must answer exactly the range asked for, and its body must
/// fill `dst` to the byte: a range answered otherwise, or a body short or long of it, is an
/// error, never a partly filled buffer passed on as whole. `dst` comes back with every outcome
/// but one: a local file's read on a blocking thread that never returned, which took it.
pub async fn read_range_into<D>(
    store: &dyn ObjectStore,
    location: &Path,
    range: Range<u64>,
    mut dst: D,
) -> (Option<D>, object_store::Result<()>)
where
    D: AsMut<[u8]> + Send + 'static,
{
    let len = dst.as_mut().len() as u64;
    if range.end.checked_sub(range.start) != Some(len) {
        let refused = generic(format!(
            "{location}: bytes {}..{} read into a buffer of {len}",
            range.start, range.end
        ));
        return (Some(dst), Err(refused));
    }
    if len == 0 {
        return (Some(dst), Ok(()));
    }
    let options = GetOptions {
        range: Some(GetRange::Bounded(range.clone())),
        ..GetOptions::default()
    };
    let result = match store.get_opts(location, options).await {
        Ok(result) => result,
        Err(e) => return (Some(dst), Err(e)),
    };
    if result.range != range {
        let answered = generic(format!(
            "{location}: the store answered bytes {:?} for {range:?}",
            result.range
        ));
        return (Some(dst), Err(answered));
    }
    match result.payload {
        GetResultPayload::File(file, path) => read_file_into(file, path, range.start, dst).await,
        GetResultPayload::Stream(body) => {
            let outcome = fill(body, dst.as_mut(), location, &range).await;
            (Some(dst), outcome)
        }
    }
}

/// Copy `body` into `dst` piece by piece as it arrives. It must fill `dst` exactly.
async fn fill<S>(
    body: S,
    dst: &mut [u8],
    location: &Path,
    range: &Range<u64>,
) -> object_store::Result<()>
where
    S: Stream<Item = object_store::Result<Bytes>>,
{
    let mut body = std::pin::pin!(body);
    let mut filled = 0usize;
    while let Some(piece) = body.try_next().await? {
        let end = filled + piece.len();
        if end > dst.len() {
            return Err(generic(format!(
                "{location}: the body of {range:?} ran past its {} bytes",
                dst.len()
            )));
        }
        dst[filled..end].copy_from_slice(&piece);
        filled = end;
    }
    if filled != dst.len() {
        return Err(generic(format!(
            "{location}: short read: {filled} of {} bytes of {range:?}",
            dst.len()
        )));
    }
    Ok(())
}

/// A local file's range read straight into `dst`, on a blocking thread.
async fn read_file_into<D>(
    mut file: std::fs::File,
    path: std::path::PathBuf,
    start: u64,
    mut dst: D,
) -> (Option<D>, object_store::Result<()>)
where
    D: AsMut<[u8]> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        let read = file
            .seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(dst.as_mut()));
        let outcome = read.map_err(|e| {
            generic(format!(
                "{}: reading {} bytes at {start}: {e}",
                path.display(),
                dst.as_mut().len()
            ))
        });
        (dst, outcome)
    })
    .await;
    match joined {
        Ok((dst, outcome)) => (Some(dst), outcome),
        // The blocking task owned the buffer, and a task that never returned took it with it.
        Err(e) => (
            None,
            Err(generic(format!("the local read's thread failed: {e}"))),
        ),
    }
}

fn generic(message: String) -> object_store::Error {
    object_store::Error::Generic {
        store: "moruna",
        source: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{ObjectStoreExt, PutPayload};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn a_range_of_a_stored_object_fills_the_buffer() {
        let store = object_store::memory::InMemory::new();
        let path = Path::from("a/object");
        let data = pattern(100_000);
        runtime().block_on(async {
            store
                .put(&path, PutPayload::from(data.clone()))
                .await
                .expect("put");
            let (dst, outcome) =
                read_range_into(&store, &path, 1_000..61_000, vec![0u8; 60_000]).await;
            outcome.expect("the range is read");
            assert_eq!(dst.expect("the buffer comes back"), data[1_000..61_000]);
        });
    }

    #[test]
    fn a_local_file_is_read_straight_into_the_buffer() {
        let dir = std::env::temp_dir().join(format!("moruna-body-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        let data = pattern(50_000);
        std::fs::write(dir.join("object"), &data).expect("the object");
        let store = object_store::local::LocalFileSystem::new_with_prefix(&dir).expect("a store");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            let path = Path::from("object");
            let (dst, outcome) =
                read_range_into(&store, &path, 4_097..44_097, vec![0u8; 40_000]).await;
            outcome.expect("the range is read");
            assert_eq!(dst.expect("the buffer comes back"), data[4_097..44_097]);
            let (dst, outcome) = read_range_into(&store, &path, 0..0, Vec::new()).await;
            outcome.expect("an empty range asks nothing");
            assert!(dst.expect("the buffer comes back").is_empty());
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_buffer_of_another_length_is_refused_and_given_back() {
        let store = object_store::memory::InMemory::new();
        let (dst, outcome) = runtime().block_on(read_range_into(
            &store,
            &Path::from("x"),
            0..10,
            vec![0u8; 9],
        ));
        let error = outcome.expect_err("nine bytes do not hold ten");
        assert!(error.to_string().contains("buffer of 9"), "{error}");
        assert_eq!(dst.expect("the buffer comes back").len(), 9);
    }

    #[test]
    fn a_store_error_is_passed_on_with_the_buffer() {
        let store = object_store::memory::InMemory::new();
        let (dst, outcome) = runtime().block_on(read_range_into(
            &store,
            &Path::from("missing"),
            0..4,
            vec![0u8; 4],
        ));
        assert!(matches!(outcome, Err(object_store::Error::NotFound { .. })));
        assert!(dst.is_some());
    }

    #[test]
    fn a_body_must_fill_the_buffer_exactly() {
        let path = Path::from("p");
        let range = 0..8;
        let pieces = |sizes: &[usize]| {
            let chunks: Vec<object_store::Result<Bytes>> = sizes
                .iter()
                .map(|n| Ok(Bytes::from(vec![9u8; *n])))
                .collect();
            futures::stream::iter(chunks)
        };
        let rt = runtime();
        let mut dst = [0u8; 8];
        rt.block_on(fill(pieces(&[3, 5]), &mut dst, &path, &range))
            .expect("two pieces fill it");
        assert_eq!(dst, [9u8; 8]);
        let short = rt
            .block_on(fill(pieces(&[3, 4]), &mut dst, &path, &range))
            .expect_err("seven of eight");
        assert!(short.to_string().contains("short read: 7 of 8"), "{short}");
        let long = rt
            .block_on(fill(pieces(&[5, 5]), &mut dst, &path, &range))
            .expect_err("ten of eight");
        assert!(long.to_string().contains("ran past"), "{long}");
        let failed = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"abc")),
            Err(generic("connection reset by peer".into())),
        ]);
        let reset = rt
            .block_on(fill(failed, &mut dst, &path, &range))
            .expect_err("the connection went");
        assert!(reset.to_string().contains("reset"), "{reset}");
    }
}
