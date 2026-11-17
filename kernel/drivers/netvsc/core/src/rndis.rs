//! RNDIS messages as bytes, laid out as Microsoft's Remote NDIS pages state them, in
//! `netvsp_protocol`'s types (`rndisprot`). Every message starts with its type and its
//! whole length; the other offsets in a message count from the field after those two.

use netvsp_protocol::rndisprot;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::Shared;

/// Bytes of the header every message starts with: its type and its length.
pub(crate) const HEADER: usize = size_of::<rndisprot::MessageHeader>();
/// Bytes before the frame in a data message this driver sends: the header and the
/// fields of REMOTE_NDIS_PACKET_MSG, with no out-of-band and no per-packet data.
pub(crate) const DATA: usize = HEADER + size_of::<rndisprot::Packet>();
/// Bytes of the longest control message this driver sends: a set of a four-byte value.
pub(crate) const CONTROL: usize = HEADER + size_of::<rndisprot::SetRequest>() + 4;

/// The control message `message_type` with the fields `body` and, after them, the
/// information buffer.
pub(crate) fn control<T: IntoBytes + Immutable>(
    message_type: u32,
    body: &T,
    information: &[u8],
) -> ([u8; CONTROL], usize) {
    // No request is longer than a set, and only a set carries information.
    const { assert!(size_of::<T>() <= size_of::<rndisprot::SetRequest>()) };
    let fields = HEADER + size_of::<T>();
    let len = fields + information.len();
    let mut bytes = [0; CONTROL];
    let header = rndisprot::MessageHeader {
        message_type,
        message_length: len as u32,
    };
    bytes[..HEADER].copy_from_slice(header.as_bytes());
    bytes[HEADER..fields].copy_from_slice(body.as_bytes());
    bytes[fields..len].copy_from_slice(information);
    (bytes, len)
}

/// What goes before a frame of `frame` bytes: REMOTE_NDIS_PACKET_MSG with the frame
/// right after its fields. The data's offset counts from the DataOffset field; the
/// out-of-band and per-packet fields are zero when a message carries neither; the
/// handle and the reserved field are zero (Microsoft, REMOTE_NDIS_PACKET_MSG).
pub(crate) fn data(frame: usize) -> [u8; DATA] {
    let header = rndisprot::MessageHeader {
        message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
        message_length: (DATA + frame) as u32,
    };
    let packet = rndisprot::Packet {
        data_offset: size_of::<rndisprot::Packet>() as u32,
        data_length: frame as u32,
        oob_data_offset: 0,
        oob_data_length: 0,
        num_oob_data_elements: 0,
        per_packet_info_offset: 0,
        per_packet_info_length: 0,
        vc_handle: 0,
        reserved: 0,
    };
    let mut bytes = [0; DATA];
    bytes[..HEADER].copy_from_slice(header.as_bytes());
    bytes[HEADER..].copy_from_slice(packet.as_bytes());
    bytes
}

/// The value at `at` in shared memory. The caller has checked it lies inside.
pub(crate) fn read<T: FromBytes + IntoBytes>(memory: &impl Shared, at: usize) -> T {
    let mut value = T::new_zeroed();
    memory.read(at, value.as_mut_bytes());
    value
}

/// The fields after the header of the message in `message`, when it is long enough to
/// hold them.
pub(crate) fn fields<T: FromBytes>(message: &[u8]) -> Option<T> {
    let (fields, _) = T::read_from_prefix(message.get(HEADER..)?).ok()?;
    Some(fields)
}
