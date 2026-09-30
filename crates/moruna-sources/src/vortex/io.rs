//! The byte path of the Vortex source (e.7, e.8, l).
//!
//! The Vortex reader asks for bytes through its `VortexReadAt` trait, whose futures are
//! `'static` and so cannot borrow the `&dyn Allocator` a read receives. The reader is therefore
//! handed a [`ChannelRead`] that only forwards each request down a channel, and the read itself
//! answers the requests while it drives the Vortex scan on the same thread ([`drive`]): the
//! answering side borrows the allocator and the reactor for as long as the scan runs, allocates
//! an arena buffer per request, has the reactor land the bytes in it, and hands Vortex a
//! `ByteBuffer` over that very buffer. Nothing is copied before decoding, and a column in a
//! canonical encoding is decoded as a view over those bytes (e.8).

use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::channel::{mpsc, oneshot};
use futures::future::{BoxFuture, Either};
use futures::{FutureExt, StreamExt};
use moruna_kernel::{MorunaError, Result};
use vortex::array::buffer::BufferHandle;
use vortex::buffer::{Alignment, ByteBuffer};
use vortex::error::{VortexError, VortexResult, vortex_err};
use vortex::io::VortexReadAt;
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;

/// How many requests the Vortex reader may keep in flight against one file; every one of them
/// is answered concurrently by [`drive`].
const CONCURRENCY: usize = 8;

/// One ranged read the Vortex reader asked for.
pub(crate) struct Request {
    offset: u64,
    length: usize,
    alignment: Alignment,
    reply: oneshot::Sender<VortexResult<BufferHandle>>,
}

/// The `VortexReadAt` a read hands the Vortex reader: every request goes down the channel to
/// the answering side of [`drive`].
pub(crate) struct ChannelRead {
    size: u64,
    uri: Arc<str>,
    requests: mpsc::UnboundedSender<Request>,
}

impl VortexReadAt for ChannelRead {
    fn uri(&self) -> Option<&Arc<str>> {
        Some(&self.uri)
    }

    fn concurrency(&self) -> usize {
        CONCURRENCY
    }

    fn size(&self) -> BoxFuture<'static, VortexResult<u64>> {
        let size = self.size;
        async move { Ok(size) }.boxed()
    }

    fn read_at(
        &self,
        offset: u64,
        length: usize,
        alignment: Alignment,
    ) -> BoxFuture<'static, VortexResult<BufferHandle>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.requests.unbounded_send(Request {
            offset,
            length,
            alignment,
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(vortex_err!(
                    "bytes {offset}..{} were asked for after the read ended",
                    offset + length as u64
                ));
            }
            answer
                .await
                .map_err(|_| vortex_err!("the read ended before bytes {offset}.. arrived"))?
        }
        .boxed()
    }
}

/// Bytes the answering side fetched: exactly the requested range, as a `ByteBuffer` aligned as
/// the request asked.
pub(crate) type Fetched = Result<ByteBuffer>;

/// What [`drive`] returns: `Err` when fetching bytes failed (the error this crate raised, which
/// is what the caller reports, rather than Vortex's rewording of it), otherwise the outcome of
/// the Vortex work itself.
pub(crate) type Driven<T> = Result<std::result::Result<T, VortexError>>;

/// Run `work` over a [`ChannelRead`] of a `size`-byte file on `runtime`, answering every
/// request it makes with `fetch(offset, length, alignment)`, until `work` resolves.
pub(crate) fn drive<T, W, WFut, F, FFut>(
    runtime: &CurrentThreadRuntime,
    uri: &str,
    size: u64,
    work: W,
    fetch: F,
) -> Driven<T>
where
    W: FnOnce(Arc<dyn VortexReadAt>) -> WFut,
    WFut: Future<Output = VortexResult<T>>,
    F: Fn(u64, usize, Alignment) -> FFut,
    FFut: Future<Output = Fetched>,
{
    let (requests, incoming) = mpsc::unbounded::<Request>();
    let reader: Arc<dyn VortexReadAt> = Arc::new(ChannelRead {
        size,
        uri: Arc::from(uri),
        requests,
    });
    let failure: Mutex<Option<MorunaError>> = Mutex::new(None);
    let outcome = runtime.block_on(async {
        let answer = incoming.for_each_concurrent(None, |request| {
            let failure = &failure;
            let fetch = &fetch;
            async move {
                // Once one request has failed the read has failed: nothing more is allocated
                // or asked of the reactor for it.
                if failure.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                    let _ = request
                        .reply
                        .send(Err(vortex_err!("an earlier request of this read failed")));
                    return;
                }
                let fetched = fetch(request.offset, request.length, request.alignment);
                let reply = match fetched.await {
                    Ok(bytes) => Ok(BufferHandle::new_host(bytes)),
                    Err(e) => {
                        let text = e.to_string();
                        let mut slot = failure.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        Err(vortex_err!("{text}"))
                    }
                };
                // The reader may have stopped waiting (a sibling request failed); nothing is
                // owed to it then.
                let _ = request.reply.send(reply);
            }
        });
        let work = work(reader);
        futures::pin_mut!(answer);
        futures::pin_mut!(work);
        match futures::future::select(work, answer).await {
            Either::Left((outcome, _)) => outcome,
            // The channel closes only when every reader is gone, which is when the work is.
            Either::Right(((), work)) => work.await,
        }
    });
    match outcome {
        Ok(value) => Ok(Ok(value)),
        Err(e) => match failure.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(fetch_error) => Err(fetch_error),
            None => Ok(Err(e)),
        },
    }
}

/// A `ByteBuffer` over `bytes` with the alignment a request asked for. An arena buffer's
/// pointer keeps the file offset's alignment within the page, and Vortex aligns every segment
/// in the file, so the buffer is used as it is; the copy below is for an alignment wider than
/// the arena's own, which no writer this crate has met produces.
pub(crate) fn aligned(
    bytes: moruna_kernel::arrow::buffer::Buffer,
    alignment: Alignment,
) -> ByteBuffer {
    if alignment.is_ptr_aligned(bytes.as_ptr()) {
        ByteBuffer::from_arrow_buffer(bytes, alignment)
    } else {
        tracing::warn!(
            target: "source.read",
            alignment = alignment.as_usize(),
            "a Vortex segment wider-aligned than its arena buffer is copied before decoding",
        );
        ByteBuffer::copy_from_aligned(bytes.as_slice(), alignment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_after_the_read_is_refused() {
        let (requests, incoming) = mpsc::unbounded::<Request>();
        let reader = ChannelRead {
            size: 10,
            uri: Arc::from("x"),
            requests,
        };
        assert_eq!(reader.uri().map(|u| u.as_ref()), Some("x"));
        assert_eq!(reader.concurrency(), CONCURRENCY);
        let runtime = CurrentThreadRuntime::new();
        assert_eq!(runtime.block_on(reader.size()).ok(), Some(10));
        drop(incoming);
        let refused = runtime.block_on(reader.read_at(0, 4, Alignment::none()));
        assert!(refused.is_err());
    }

    #[test]
    fn an_unanswered_request_is_an_error() {
        let (requests, mut incoming) = mpsc::unbounded::<Request>();
        let reader = ChannelRead {
            size: 10,
            uri: Arc::from("x"),
            requests,
        };
        let runtime = CurrentThreadRuntime::new();
        let pending = reader.read_at(0, 4, Alignment::none());
        let request = runtime.block_on(incoming.next());
        drop(request);
        assert!(runtime.block_on(pending).is_err());
    }

    #[test]
    fn a_misaligned_buffer_is_realigned() {
        let bytes = moruna_kernel::arrow::buffer::Buffer::from_vec(vec![1u8; 64]);
        let odd = bytes.slice_with_length(1, 32);
        let buffer = aligned(odd, Alignment::new(8));
        assert!(Alignment::new(8).is_ptr_aligned(buffer.as_ptr()));
        assert_eq!(buffer.len(), 32);
    }

    #[test]
    fn a_fetch_failure_is_what_the_caller_sees() {
        let runtime = CurrentThreadRuntime::new();
        let driven: Driven<u8> = drive(
            &runtime,
            "mem",
            8,
            |reader| async move {
                reader.read_at(0, 4, Alignment::none()).await?;
                Ok(1u8)
            },
            |_, _, _| async { Err(MorunaError::Plan("no bytes here".into())) },
        );
        assert!(matches!(driven, Err(MorunaError::Plan(_))));
        let fine: Driven<u8> = drive(
            &runtime,
            "mem",
            8,
            |_| async { Err(vortex_err!("vortex says no")) },
            |_, _, _| async { Ok(ByteBuffer::copy_from(vec![0u8; 4])) },
        );
        assert!(matches!(fine, Ok(Err(_))));
    }
}
