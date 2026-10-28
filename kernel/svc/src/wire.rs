//! The wire format of a message's body (SDD 23, e.2): what a protocol's fields encode
//! to, and the handles that travel with them.
//!
//! Everything is little endian and fixed width: integers as themselves, `bool` as one
//! byte (0 or 1), a length or count as a `u32` before its items, `Option` as a tag byte
//! (0 or 1) before its value, a tuple as its fields in order. A handle is a one-byte
//! index into the message's handle list, so the bytes say where each handle goes and
//! the kernel moves the handles themselves. A decoder refuses truncation, a bad tag, a
//! bad UTF-8 string, an index that names no handle or one already taken, and, at the
//! end, bytes or handles left over: a message means exactly one thing or nothing.

use alloc::{string::String, vec::Vec};

use moruna_abi::{ObjectType, limits};

use crate::{
    frame::HEADER,
    os::{Channel, Handle},
};

/// Why a body could not be encoded or decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The body ended before a field did.
    Truncated,
    /// A field holds a value its type does not have (a tag, a bool, UTF-8, a count).
    Invalid,
    /// A handle index names no handle of the message, or one already taken, or a
    /// handle of the wrong type.
    Handle,
    /// Bytes or handles were left over after the last field.
    Trailing,
    /// The message would not fit a channel: more than `limits::CHANNEL_MAX_BYTES`
    /// bytes or `limits::CHANNEL_MAX_HANDLES` handles.
    TooLarge,
}

/// A message under construction: its header room, its body, and the handles it moves.
pub struct Encoder {
    bytes: Vec<u8>,
    handles: Vec<Option<Handle>>,
}

impl Default for Encoder {
    fn default() -> Encoder {
        Encoder::new()
    }
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder {
            bytes: alloc::vec![0u8; HEADER],
            handles: Vec::new(),
        }
    }

    /// The body's length so far, in bytes.
    pub fn body_len(&self) -> usize {
        self.bytes.len() - HEADER
    }

    /// The number of handles so far.
    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    /// Appends raw bytes, with no length before them.
    pub fn put_raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Appends a handle: its index in the message goes in the body.
    pub fn put_handle(&mut self, handle: Handle) -> Result<(), WireError> {
        if self.handles.len() >= limits::CHANNEL_MAX_HANDLES {
            return Err(WireError::TooLarge);
        }
        self.bytes.push(self.handles.len() as u8);
        self.handles.push(Some(handle));
        Ok(())
    }

    /// Appends a `u32` length or count, refusing one that no message could hold.
    pub fn put_len(&mut self, len: usize) -> Result<(), WireError> {
        if len > limits::CHANNEL_MAX_BYTES {
            return Err(WireError::TooLarge);
        }
        self.put_raw(&(len as u32).to_le_bytes());
        Ok(())
    }

    /// The whole message (header room included) and its handles, for the frame layer.
    pub(crate) fn into_parts(self) -> (Vec<u8>, Vec<Option<Handle>>) {
        (self.bytes, self.handles)
    }
}

/// A received body being read field by field.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    handles: &'a mut [Option<Handle>],
}

impl<'a> Decoder<'a> {
    /// A decoder over `bytes` (the body, without the header) and the message's handles.
    pub fn new(bytes: &'a [u8], handles: &'a mut [Option<Handle>]) -> Decoder<'a> {
        Decoder {
            bytes,
            at: 0,
            handles,
        }
    }

    /// The bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// The next `n` bytes.
    pub fn take_raw(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.checked_add(n).ok_or(WireError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(WireError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take_raw(N)?);
        Ok(out)
    }

    /// A `u32` length or count, refused beyond what a message can hold.
    pub fn take_len(&mut self) -> Result<usize, WireError> {
        let n = u32::from_le_bytes(self.array()?) as usize;
        if n > limits::CHANNEL_MAX_BYTES {
            return Err(WireError::Invalid);
        }
        Ok(n)
    }

    /// The handle the next byte names, taken out of the message.
    pub fn take_handle(&mut self) -> Result<Handle, WireError> {
        let [index] = self.array::<1>()?;
        self.handles
            .get_mut(index as usize)
            .and_then(Option::take)
            .ok_or(WireError::Handle)
    }

    /// Handle `index` of the message, taken out of it, whatever the body says.
    pub(crate) fn take_slot(&mut self, index: usize) -> Option<Handle> {
        self.handles.get_mut(index).and_then(Option::take)
    }

    /// Succeeds only if every byte was read and every handle taken.
    pub fn finish(&self) -> Result<(), WireError> {
        if self.at != self.bytes.len() || self.handles.iter().any(Option::is_some) {
            return Err(WireError::Trailing);
        }
        Ok(())
    }
}

/// A type that travels in a message. Encoding consumes the value, because a handle in
/// it moves.
pub trait Wire: Sized {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError>;
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError>;
}

macro_rules! integer {
    ($($t:ty),*) => {$(
        impl Wire for $t {
            fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
                e.put_raw(&self.to_le_bytes());
                Ok(())
            }
            fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
                Ok(<$t>::from_le_bytes(d.array()?))
            }
        }
    )*};
}

integer!(u8, u16, u32, u64, i8, i16, i32, i64);

impl Wire for bool {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_raw(&[u8::from(self)]);
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        match d.array::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(WireError::Invalid),
        }
    }
}

impl Wire for () {
    fn encode(self, _: &mut Encoder) -> Result<(), WireError> {
        Ok(())
    }
    fn decode(_: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(())
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.len())?;
        for item in self {
            item.encode(e)?;
        }
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        let mut items = Vec::with_capacity(n.min(d.remaining()));
        for _ in 0..n {
            items.push(T::decode(d)?);
        }
        Ok(items)
    }
}

impl Wire for String {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.len())?;
        e.put_raw(self.as_bytes());
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        let bytes = d.take_raw(n)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| WireError::Invalid)
    }
}

impl<T: Wire> Wire for Option<T> {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        match self {
            None => {
                e.put_raw(&[0]);
                Ok(())
            }
            Some(v) => {
                e.put_raw(&[1]);
                v.encode(e)
            }
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        match d.array::<1>()? {
            [0] => Ok(None),
            [1] => T::decode(d).map(Some),
            _ => Err(WireError::Invalid),
        }
    }
}

macro_rules! tuple {
    ($($name:ident),+) => {
        impl<$($name: Wire),+> Wire for ($($name,)+) {
            #[allow(non_snake_case)]
            fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
                let ($($name,)+) = self;
                $($name.encode(e)?;)+
                Ok(())
            }
            fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
                Ok(($($name::decode(d)?,)+))
            }
        }
    };
}

tuple!(A);
tuple!(A, B);
tuple!(A, B, C);
tuple!(A, B, C, D);

impl Wire for Handle {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_handle(self)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        d.take_handle()
    }
}

/// Takes a handle and checks its object is of type `kind`.
fn typed_handle(d: &mut Decoder<'_>, kind: ObjectType) -> Result<Handle, WireError> {
    let handle = d.take_handle()?;
    match handle.info() {
        Ok((k, _)) if k == kind => Ok(handle),
        _ => Err(WireError::Handle),
    }
}

impl Wire for Channel {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_handle(self.0)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        typed_handle(d, ObjectType::Channel).map(Channel)
    }
}

#[cfg(target_os = "none")]
impl Wire for moruna_sys::Memory {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_handle(self.0)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        typed_handle(d, ObjectType::Memory).map(moruna_sys::Memory)
    }
}
