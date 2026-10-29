//! The hand-built compressed streams that lie (CT-T24, CT-T27): shared by the tests that refuse
//! them with the reason and the one that measures the heap while they do.

#![allow(dead_code)]

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::{BodyCompressionMethod, CompressionType, MetadataVersion};
use lz4_flex::frame::BlockSize;
use moruna_testkit::ipc::crafted::{
    How, lz4_frame, packed, says, stream_as, zstd_frame, zstd_frame_without_size,
};

pub const CODECS: [(&str, CompressionType); 2] = [
    ("zstd", CompressionType::ZSTD),
    ("lz4", CompressionType::LZ4_FRAME),
];

/// A schema of one column.
pub fn one(name: &str, data_type: DataType) -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new(name, data_type, true)]))
}

pub fn offsets32(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn how(codec: CompressionType) -> How {
    How {
        codec,
        method: BodyCompressionMethod::BUFFER,
        version: MetadataVersion::V5,
    }
}

/// `values` as the one `Int64` column of a batch, honestly packed.
pub fn int64s(codec: CompressionType, values: &[i64]) -> Vec<u8> {
    let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    stream_as(
        &one("v", DataType::Int64),
        how(codec),
        values.len() as i64,
        &[(values.len() as i64, 0)],
        &[Vec::new(), packed(codec, &raw)],
    )
}

/// A string column of `n` rows with these (honestly packed) offsets and data buffers.
pub fn strings(codec: CompressionType, n: i64, offsets: Vec<u8>, data: Vec<u8>) -> Vec<u8> {
    stream_as(
        &one("s", DataType::Utf8),
        how(codec),
        n,
        &[(n, 0)],
        &[Vec::new(), offsets, data],
    )
}

/// One refusal case: the stream, the ceiling, and what the refusal must say.
pub struct Bomb {
    pub label: String,
    pub bytes: Vec<u8>,
    pub ceiling: u64,
    pub says: &'static str,
}

pub fn bombs() -> Vec<Bomb> {
    let mut out = Vec::new();
    let mut add = |label: &str, bytes: Vec<u8>, ceiling: u64, says: &'static str| {
        out.push(Bomb {
            label: label.to_string(),
            bytes,
            ceiling,
            says,
        });
    };
    let thirty_two = [0u8; 32];
    let int64_with = |codec, values: Vec<u8>| {
        stream_as(
            &one("v", DataType::Int64),
            how(codec),
            4,
            &[(4, 0)],
            &[Vec::new(), values],
        )
    };
    for (name, codec) in CODECS {
        let frame = |raw: &[u8]| {
            if codec == CompressionType::ZSTD {
                zstd_frame(raw)
            } else {
                lz4_frame(raw, BlockSize::Max64KB)
            }
        };
        let label = |what: &str| format!("{name}: {what}");

        // A buffer declaring a terabyte of four values.
        add(
            &label("1 TiB"),
            int64_with(codec, says(1 << 40, &frame(&thirty_two))),
            1 << 20,
            "declares 1099511627776 uncompressed bytes where the message implies 32",
        );
        // One byte more, and one byte less, than the 32 the column needs.
        for declared in [33, 31] {
            add(
                &label(&format!("declared {declared}")),
                int64_with(codec, says(declared, &frame(&thirty_two))),
                1 << 20,
                "where the message implies 32",
            );
        }
        add(
            &label("negative length"),
            int64_with(codec, says(-2, &frame(&thirty_two))),
            1 << 20,
            "declares an uncompressed length of -2",
        );
        add(
            &label("a length that is not 8 bytes"),
            int64_with(codec, vec![1, 2, 3]),
            1 << 20,
            "cannot hold its 8-byte length",
        );
        // The validity bitmap is ceil(4 / 8) = 1 byte, or none.
        add(
            &label("a validity bitmap of a gigabyte"),
            stream_as(
                &one("v", DataType::Int64),
                how(codec),
                4,
                &[(4, 0)],
                &[says(1 << 30, &frame(&[0xff])), packed(codec, &thirty_two)],
            ),
            1 << 20,
            "validity buffer that declares 1073741824 uncompressed bytes where the message implies 1",
        );

        // Offsets that go backwards, that start somewhere, and that end beyond the ceiling.
        add(
            &label("offsets not monotonic"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 5, 2, 7])),
                packed(codec, b"abcdefg"),
            ),
            1 << 20,
            "are not monotonic: offset 2 is 2 after 5",
        );
        add(
            &label("offsets that start at 1"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[1, 2, 3, 4])),
                packed(codec, b"abc"),
            ),
            1 << 20,
            "start at 1, not 0",
        );
        add(
            &label("last offset beyond the ceiling"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 1, 2, 2_000_000_000])),
                says(2_000_000_000, &frame(b"abc")),
            ),
            1 << 20,
            "2000000000 uncompressed bytes, and",
        );
        add(
            &label("last offset the data buffer does not match"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 1, 2, 2_000_000_000])),
                packed(codec, b"abc"),
            ),
            1 << 20,
            "data buffer that declares 3 uncompressed bytes where the message implies 2000000000",
        );
        add(
            &label("offsets that are not there"),
            strings(codec, 3, Vec::new(), Vec::new()),
            1 << 20,
            "offsets buffer that declares 0 uncompressed bytes where the message implies 16",
        );

        // Frames that hold more, or less, than the length they were given.
        let ones = vec![1u8; 4096];
        add(
            &label("inflates past (a frame of 4096 for 32)"),
            int64_with(codec, says(32, &frame(&ones))),
            1 << 20,
            "inflates past the 32 bytes declared for it",
        );
        add(
            &label("inflates short (a frame of 8 for 32)"),
            int64_with(codec, says(32, &frame(&ones[..8]))),
            1 << 20,
            "inflates short of the 32 bytes declared for it",
        );
        add(
            &label("string data inflates past"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 1, 2, 3])),
                says(3, &frame(&ones)),
            ),
            1 << 20,
            "inflates past the 3 bytes declared for it",
        );
        add(
            &label("string data inflates short"),
            strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 1, 2, 3])),
                says(3, &frame(&ones[..1])),
            ),
            1 << 20,
            "inflates short of the 3 bytes declared for it",
        );
        add(
            &label("garbage where a frame should be"),
            int64_with(codec, says(32, &[0xAB; 40])),
            1 << 20,
            "buffer that",
        );

        // An honest column bigger than the ceiling is refused before anything is allocated.
        let millions = vec![0u8; 8_000_000];
        add(
            &label("1,000,000 values, a 1 MiB ceiling"),
            stream_as(
                &one("v", DataType::Int64),
                how(codec),
                1_000_000,
                &[(1_000_000, 0)],
                &[Vec::new(), says(8_000_000, &frame(&millions))],
            ),
            1 << 20,
            "8000000 uncompressed bytes, and 1048576 of the 1048576 bytes",
        );
        // Many columns, each inside the ceiling, together past it.
        let count = 40;
        let wide = Arc::new(Schema::new(
            (0..count)
                .map(|i| Field::new(format!("c{i}"), DataType::Int64, true))
                .collect::<Vec<_>>(),
        ));
        let sixty_four_k = vec![0u8; 65_536];
        let mut buffers = Vec::new();
        for _ in 0..count {
            buffers.push(Vec::new());
            buffers.push(says(65_536, &frame(&sixty_four_k)));
        }
        add(
            &label("forty columns of 64 KiB, a 1 MiB ceiling"),
            stream_as(&wide, how(codec), 8192, &vec![(8192, 0); count], &buffers),
            1 << 20,
            "bytes this reader may inflate for one batch are left",
        );
    }

    // zstd frames that do not say what they hold: the output is bound all the same.
    let ones = vec![1u8; 4096];
    add(
        "zstd: a frame with no content size, past",
        int64_with(
            CompressionType::ZSTD,
            says(32, &zstd_frame_without_size(&ones)),
        ),
        1 << 20,
        "inflates past the 32 bytes declared for it",
    );
    add(
        "zstd: a frame with no content size, short",
        int64_with(
            CompressionType::ZSTD,
            says(32, &zstd_frame_without_size(&ones[..8])),
        ),
        1 << 20,
        "inflates short of the 32 bytes declared for it",
    );
    // An LZ4 frame that names 4 MiB blocks costs 8 MiB to read, whatever it holds.
    add(
        "lz4: 4 MiB blocks, a 1 MiB ceiling",
        int64_with(
            CompressionType::LZ4_FRAME,
            says(32, &lz4_frame(&[0u8; 32], BlockSize::Max4MB)),
        ),
        1 << 20,
        "takes 8388608 bytes to inflate",
    );
    out
}
