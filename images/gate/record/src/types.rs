//! The logical types a gate record carries: enough of Arrow's `Schema.fbs` to read a job's input
//! columns and to write its output columns again.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A column's name, nullability and type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LField {
    pub name: String,
    pub nullable: bool,
    pub ty: LType,
}

impl LField {
    pub fn new(name: &str, ty: LType) -> LField {
        LField {
            name: String::from(name),
            nullable: true,
            ty,
        }
    }
}

/// `TimeUnit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Second,
    Milli,
    Micro,
    Nano,
}

impl Unit {
    pub fn from_fb(v: i16) -> Option<Unit> {
        Some(match v {
            0 => Unit::Second,
            1 => Unit::Milli,
            2 => Unit::Micro,
            3 => Unit::Nano,
            _ => return None,
        })
    }

    pub fn to_fb(self) -> i16 {
        match self {
            Unit::Second => 0,
            Unit::Milli => 1,
            Unit::Micro => 2,
            Unit::Nano => 3,
        }
    }
}

/// The Arrow types a gate function reads or writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LType {
    Int {
        bits: u8,
        signed: bool,
    },
    /// `FloatingPoint`: 16, 32 or 64 bits.
    Float {
        bits: u8,
    },
    Bool,
    Utf8,
    LargeUtf8,
    Binary,
    LargeBinary,
    Timestamp {
        unit: Unit,
        tz: Option<String>,
    },
    FixedSizeList {
        size: i32,
        child: Box<LField>,
    },
    List {
        child: Box<LField>,
    },
    LargeList {
        child: Box<LField>,
    },
    Struct(Vec<LField>),
    /// A map: its one child is the entries struct.
    Map {
        child: Box<LField>,
        sorted: bool,
    },
    /// A type a gate function never touches, by its `Type` id: carried by name only.
    Other(u8),
}

impl LType {
    pub const I64: LType = LType::Int {
        bits: 64,
        signed: true,
    };
    pub const I32: LType = LType::Int {
        bits: 32,
        signed: true,
    };
    pub const U32: LType = LType::Int {
        bits: 32,
        signed: false,
    };
    pub const U64: LType = LType::Int {
        bits: 64,
        signed: false,
    };
    pub const F64: LType = LType::Float { bits: 64 };

    /// The width of one value of a fixed-width type, in bytes.
    pub fn width(&self) -> Option<usize> {
        match self {
            LType::Int { bits, .. } | LType::Float { bits } => Some(usize::from(*bits) / 8),
            LType::Timestamp { .. } => Some(8),
            _ => None,
        }
    }
}
