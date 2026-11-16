//! A small flatbuffer writer, for the two messages of a record (`Message.fbs`, `Schema.fbs`).
//!
//! It writes front to back: a table's vtable, then the table, then the objects its offset fields
//! point to, so every `uoffset` points forward. Scalars are placed at their natural alignment
//! from the buffer's start, which is 8-aligned in a message (after its 8-byte prefix), as
//! flatbuffers' verifier requires.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A field's value.
pub enum Val {
    U8(u8),
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    Str(String),
    Table(Box<Obj>),
    Tables(Vec<Obj>),
    /// A vector of 16-byte structs of two `i64`: `FieldNode`s or `Buffer`s.
    Pairs(Vec<(i64, i64)>),
}

impl Val {
    /// The inline size: the scalar's, or a 4-byte offset.
    fn size(&self) -> usize {
        match self {
            Val::U8(_) | Val::Bool(_) => 1,
            Val::I16(_) => 2,
            Val::I32(_) => 4,
            Val::I64(_) => 8,
            Val::Str(_) | Val::Table(_) | Val::Tables(_) | Val::Pairs(_) => 4,
        }
    }

    fn is_offset(&self) -> bool {
        matches!(
            self,
            Val::Str(_) | Val::Table(_) | Val::Tables(_) | Val::Pairs(_)
        )
    }
}

/// A table: its fields by slot.
#[derive(Default)]
pub struct Obj {
    fields: Vec<(u16, Val)>,
}

impl Obj {
    pub fn new() -> Obj {
        Obj::default()
    }

    pub fn with(mut self, slot: u16, v: Val) -> Obj {
        self.fields.push((slot, v));
        self
    }
}

/// The finished flatbuffer of `root`, padded to a multiple of 8.
pub fn finish(root: Obj) -> Vec<u8> {
    let mut b = Vec::with_capacity(256);
    b.extend_from_slice(&[0; 4]);
    let at = table(&mut b, root);
    patch(&mut b, 0, at);
    pad(&mut b, 8, 0);
    b
}

/// Pads `b` with zeros until `(b.len() + extra) % align == 0`.
fn pad(b: &mut Vec<u8>, align: usize, extra: usize) {
    while !(b.len() + extra).is_multiple_of(align) {
        b.push(0);
    }
}

/// Points the `uoffset` at `from` to `to`.
fn patch(b: &mut [u8], from: usize, to: usize) {
    let v = (to - from) as u32;
    b[from..from + 4].copy_from_slice(&v.to_le_bytes());
}

fn table(b: &mut Vec<u8>, obj: Obj) -> usize {
    let mut fields = obj.fields;
    // Largest first, so each scalar is aligned once the table's first field is.
    fields.sort_by_key(|f| core::cmp::Reverse(f.1.size()));
    let max_slot = fields
        .iter()
        .map(|(s, _)| *s)
        .max()
        .map_or(0, |s| s as usize + 1);
    let mut place = vec![0u16; max_slot];
    let mut cursor = 4usize;
    for (slot, v) in &fields {
        place[*slot as usize] = cursor as u16;
        cursor += v.size();
    }
    let table_size = cursor;
    // The vtable, 2-aligned (the buffer is kept 4-aligned).
    pad(b, 4, 0);
    let vt = b.len();
    b.extend_from_slice(&((4 + 2 * max_slot) as u16).to_le_bytes());
    b.extend_from_slice(&(table_size as u16).to_le_bytes());
    for p in &place {
        b.extend_from_slice(&p.to_le_bytes());
    }
    // The table: its first field 8-aligned when it has an 8-byte one.
    let wide = fields.iter().any(|(_, v)| v.size() == 8);
    if wide {
        pad(b, 8, 4);
    } else {
        pad(b, 4, 0);
    }
    let at = b.len();
    b.extend_from_slice(&((at - vt) as i32).to_le_bytes());
    let mut offsets = Vec::new();
    for (_, v) in fields {
        let here = b.len();
        match v {
            Val::U8(x) => b.push(x),
            Val::Bool(x) => b.push(x as u8),
            Val::I16(x) => b.extend_from_slice(&x.to_le_bytes()),
            Val::I32(x) => b.extend_from_slice(&x.to_le_bytes()),
            Val::I64(x) => b.extend_from_slice(&x.to_le_bytes()),
            other => {
                b.extend_from_slice(&[0; 4]);
                offsets.push((here, other));
            }
        }
    }
    pad(b, 4, 0);
    for (from, v) in offsets {
        debug_assert!(v.is_offset());
        let to = object(b, v);
        patch(b, from, to);
    }
    at
}

/// Writes the object an offset field points to; where it starts.
fn object(b: &mut Vec<u8>, v: Val) -> usize {
    match v {
        Val::Str(s) => {
            pad(b, 4, 0);
            let at = b.len();
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
            b.push(0);
            pad(b, 4, 0);
            at
        }
        Val::Table(t) => table(b, *t),
        Val::Tables(ts) => {
            pad(b, 4, 0);
            let at = b.len();
            b.extend_from_slice(&(ts.len() as u32).to_le_bytes());
            let slots = b.len();
            b.extend_from_slice(&vec![0u8; 4 * ts.len()]);
            for (i, t) in ts.into_iter().enumerate() {
                let to = table(b, t);
                patch(b, slots + 4 * i, to);
            }
            at
        }
        Val::Pairs(ps) => {
            // The elements 8-aligned, after the 4-byte count.
            pad(b, 8, 4);
            let at = b.len();
            b.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for (x, y) in ps {
                b.extend_from_slice(&x.to_le_bytes());
                b.extend_from_slice(&y.to_le_bytes());
            }
            at
        }
        scalar => {
            // Not an offset field: never reached, as `table` writes scalars inline.
            let _ = scalar;
            b.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_ipc_check::fb::Table;

    #[test]
    fn reads_back() {
        let child = Obj::new().with(0, Val::I32(7));
        let root = Obj::new()
            .with(0, Val::I16(4))
            .with(1, Val::U8(3))
            .with(3, Val::I64(-9))
            .with(4, Val::Str(String::from("name")))
            .with(5, Val::Table(Box::new(child)))
            .with(
                6,
                Val::Tables(vec![Obj::new().with(1, Val::Bool(true)), Obj::new()]),
            )
            .with(7, Val::Pairs(vec![(1, 2), (3, 4)]));
        let buf = finish(root);
        assert_eq!(buf.len() % 8, 0);
        let t = Table::root(&buf).unwrap();
        assert_eq!(t.i16(0, 0).unwrap(), 4);
        assert_eq!(t.u8(1, 0).unwrap(), 3);
        assert_eq!(t.u8(2, 9).unwrap(), 9);
        assert_eq!(t.i64(3, 0).unwrap(), -9);
        assert_eq!(t.position(3).unwrap().unwrap() % 8, 0);
        assert_eq!(t.str(4).unwrap(), Some("name"));
        assert_eq!(t.table(5).unwrap().unwrap().i32(0, 0).unwrap(), 7);
        let v = t.vector(6, 4).unwrap().unwrap();
        assert_eq!(v.len(), 2);
        assert!(v.table(0).unwrap().bool(1, false).unwrap());
        let p = t.vector(7, 16).unwrap().unwrap();
        assert_eq!(p.start() % 8, 0);
        assert_eq!(
            moruna_ipc_check::fb::pair(p.element(1).unwrap()),
            Some((3, 4))
        );
    }
}
