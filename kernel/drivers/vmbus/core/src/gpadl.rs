//! The messages that describe a GPADL to the host: guest memory, named by its page
//! numbers, that the host may then read and write.
//!
//! A GPADL here is always one range of whole pages starting at the first page's first
//! byte, which is what a ring pair and a driver's buffers are. Its description is a list
//! of 8-byte values: one for the range (its length in bytes, its offset in the first page)
//! and one page number for each page. `GpadlHeader` carries the first values and
//! `GpadlBody` messages the rest, as OpenVMM's client sends them
//! (`openvmm: vm/devices/vmbus/vmbus_client/src/lib.rs:1265-1294`), with the range value
//! built as `openvmm: vm/devices/vmbus/vmbus_client/src/driver.rs:95-98` builds it.

use alloc::vec::Vec;

use vmbus_core::OutgoingMessage;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::GpadlBody;
use vmbus_core::protocol::GpadlHeader;
use vmbus_core::protocol::GpadlId;

/// The size of a page of guest memory, as VMBus counts it.
pub const PAGE_SIZE: usize = 4096;

/// The most pages one GPADL can name, 8,190 (a little under 32 MiB). `GpadlHeader::len`
/// counts the bytes of the 8-byte values in 16 bits, so there are at most 8,191 values:
/// the one for the range, and the page numbers.
pub const MAX_PAGES: usize = u16::MAX as usize / 8 - 1;

/// The messages that create `gpadl` on `channel` over `pages`, in the order they are sent.
/// `pages` holds 1 to [`MAX_PAGES`] page numbers; the client checks that before it calls.
pub(crate) fn create(channel: ChannelId, gpadl: GpadlId, pages: &[u64]) -> Vec<OutgoingMessage> {
    debug_assert!(!pages.is_empty() && pages.len() <= MAX_PAGES);
    // The range's value is its length in the low half and its offset, zero, in the high.
    let range = (pages.len() * PAGE_SIZE) as u64;
    let values: Vec<u8> = core::iter::once(range)
        .chain(pages.iter().copied())
        .flat_map(u64::to_le_bytes)
        .collect();
    let (first, rest) = values.split_at(values.len().min(GpadlHeader::MAX_DATA_VALUES * 8));

    let header = GpadlHeader {
        channel_id: channel,
        gpadl_id: gpadl,
        len: values.len() as u16,
        count: 1,
    };
    let mut messages = Vec::with_capacity(1 + rest.len().div_ceil(GpadlBody::MAX_DATA_VALUES * 8));
    messages.push(OutgoingMessage::with_data(&header, first));
    let body = GpadlBody {
        rsvd: 0,
        gpadl_id: gpadl,
    };
    for chunk in rest.chunks(GpadlBody::MAX_DATA_VALUES * 8) {
        messages.push(OutgoingMessage::with_data(&body, chunk));
    }
    messages
}
