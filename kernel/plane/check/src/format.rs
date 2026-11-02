//! The field slots and enum values of Arrow's `Message.fbs` and `Schema.fbs` this crate reads:
//! a slot is a field's index in its table, from 0, as the vtable numbers it.

/// `Message`.
pub mod message {
    pub const VERSION: usize = 0;
    pub const HEADER_TYPE: usize = 1;
    pub const HEADER: usize = 2;
    pub const BODY_LENGTH: usize = 3;
}

/// `MessageHeader`, the union a message carries.
pub mod header {
    pub const NONE: u8 = 0;
    pub const SCHEMA: u8 = 1;
    pub const DICTIONARY_BATCH: u8 = 2;
    pub const RECORD_BATCH: u8 = 3;
    pub const TENSOR: u8 = 4;
    pub const SPARSE_TENSOR: u8 = 5;

    /// The name arrow-ipc's `Debug` gives a header type.
    pub fn name(kind: u8) -> alloc::string::String {
        let known = match kind {
            NONE => "NONE",
            SCHEMA => "Schema",
            DICTIONARY_BATCH => "DictionaryBatch",
            RECORD_BATCH => "RecordBatch",
            TENSOR => "Tensor",
            SPARSE_TENSOR => "SparseTensor",
            other => return alloc::format!("<UNKNOWN {other}>"),
        };
        alloc::string::String::from(known)
    }
}

/// `MetadataVersion`.
pub mod version {
    pub const V1: i16 = 0;
    pub const V4: i16 = 3;
    pub const V5: i16 = 4;

    /// The name arrow-ipc's `Debug` gives a version.
    pub fn name(version: i16) -> alloc::string::String {
        match version {
            0..=4 => alloc::format!("V{}", version + 1),
            other => alloc::format!("<UNKNOWN {other}>"),
        }
    }
}

/// `Schema`.
pub mod schema {
    pub const ENDIANNESS: usize = 0;
    pub const FIELDS: usize = 1;
    /// `Endianness.Big`.
    pub const BIG: i16 = 1;
}

/// `Field`.
pub mod field {
    pub const NAME: usize = 0;
    pub const NULLABLE: usize = 1;
    pub const TYPE_TYPE: usize = 2;
    pub const TYPE: usize = 3;
    pub const DICTIONARY: usize = 4;
    pub const CHILDREN: usize = 5;
}

/// `DictionaryEncoding`.
pub mod dictionary {
    pub const ID: usize = 0;
    pub const INDEX_TYPE: usize = 1;
}

/// `Type`, the union a field's type is, and the slots and enums of its tables.
pub mod types {
    pub const NONE: u8 = 0;
    pub const NULL: u8 = 1;
    pub const INT: u8 = 2;
    pub const FLOATING_POINT: u8 = 3;
    pub const BINARY: u8 = 4;
    pub const UTF8: u8 = 5;
    pub const BOOL: u8 = 6;
    pub const DECIMAL: u8 = 7;
    pub const DATE: u8 = 8;
    pub const TIME: u8 = 9;
    pub const TIMESTAMP: u8 = 10;
    pub const INTERVAL: u8 = 11;
    pub const LIST: u8 = 12;
    pub const STRUCT: u8 = 13;
    pub const UNION: u8 = 14;
    pub const FIXED_SIZE_BINARY: u8 = 15;
    pub const FIXED_SIZE_LIST: u8 = 16;
    pub const MAP: u8 = 17;
    pub const DURATION: u8 = 18;
    pub const LARGE_BINARY: u8 = 19;
    pub const LARGE_UTF8: u8 = 20;
    pub const LARGE_LIST: u8 = 21;
    pub const RUN_END_ENCODED: u8 = 22;
    pub const BINARY_VIEW: u8 = 23;
    pub const UTF8_VIEW: u8 = 24;
    pub const LIST_VIEW: u8 = 25;
    pub const LARGE_LIST_VIEW: u8 = 26;

    const NAMES: [&str; 27] = [
        "NONE",
        "Null",
        "Int",
        "FloatingPoint",
        "Binary",
        "Utf8",
        "Bool",
        "Decimal",
        "Date",
        "Time",
        "Timestamp",
        "Interval",
        "List",
        "Struct_",
        "Union",
        "FixedSizeBinary",
        "FixedSizeList",
        "Map",
        "Duration",
        "LargeBinary",
        "LargeUtf8",
        "LargeList",
        "RunEndEncoded",
        "BinaryView",
        "Utf8View",
        "ListView",
        "LargeListView",
    ];

    /// The name arrow-ipc's `Debug` gives a type id.
    pub fn name(kind: u8) -> alloc::string::String {
        match NAMES.get(usize::from(kind)) {
            Some(name) => alloc::string::String::from(*name),
            None => alloc::format!("<UNKNOWN {kind}>"),
        }
    }

    /// `Int`: `bitWidth`, `is_signed`.
    pub const INT_BIT_WIDTH: usize = 0;
    /// `FloatingPoint.precision`: `HALF`, `SINGLE`, `DOUBLE`.
    pub const PRECISION: usize = 0;
    /// `Decimal`: `precision`, `scale`, `bitWidth` (default 128).
    pub const DECIMAL_PRECISION: usize = 0;
    pub const DECIMAL_SCALE: usize = 1;
    pub const DECIMAL_BIT_WIDTH: usize = 2;
    /// `Date.unit` (default `MILLISECOND`), `Time.unit` (default `MILLISECOND`) and
    /// `Time.bitWidth` (default 32), `Timestamp.unit` (default `SECOND`), `Interval.unit`
    /// (default `YEAR_MONTH`), `Duration.unit` (default `MILLISECOND`).
    pub const UNIT: usize = 0;
    pub const TIME_BIT_WIDTH: usize = 1;
    /// `FixedSizeBinary.byteWidth`, `FixedSizeList.listSize`.
    pub const WIDTH: usize = 0;
    /// `Union`: `mode` (default `Sparse`), `typeIds`.
    pub const UNION_MODE: usize = 0;
    pub const UNION_TYPE_IDS: usize = 1;
    pub const SPARSE: i16 = 0;
    pub const DENSE: i16 = 1;
    /// `TimeUnit`.
    pub const SECOND: i16 = 0;
    pub const MILLISECOND: i16 = 1;
    pub const MICROSECOND: i16 = 2;
    pub const NANOSECOND: i16 = 3;
    /// `DateUnit`.
    pub const DAY: i16 = 0;
    /// `IntervalUnit`.
    pub const YEAR_MONTH: i16 = 0;
    pub const DAY_TIME: i16 = 1;
    pub const MONTH_DAY_NANO: i16 = 2;
}

/// `RecordBatch`.
pub mod batch {
    pub const LENGTH: usize = 0;
    pub const NODES: usize = 1;
    pub const BUFFERS: usize = 2;
    pub const COMPRESSION: usize = 3;
    pub const VARIADIC: usize = 4;
    /// A `FieldNode` or a `Buffer`: two `i64`.
    pub const ENTRY: usize = 16;
}

/// `BodyCompression`: `codec` (`LZ4_FRAME` 0, `ZSTD` 1), `method` (`BUFFER` 0).
pub mod compression {
    pub const CODEC: usize = 0;
    pub const METHOD: usize = 1;

    /// The name arrow-ipc's `Debug` gives a codec.
    pub fn codec(codec: i8) -> alloc::string::String {
        match codec {
            0 => alloc::string::String::from("LZ4_FRAME"),
            1 => alloc::string::String::from("ZSTD"),
            other => alloc::format!("<UNKNOWN {other}>"),
        }
    }
}

/// `DictionaryBatch`.
pub mod dictionary_batch {
    pub const ID: usize = 0;
    pub const DATA: usize = 1;
    pub const IS_DELTA: usize = 2;
}
