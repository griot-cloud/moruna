//! CT-T8 amb1_roundtrip: write every dtype and ndim 0 to 8 through a reference writer in the
//! test, read back, byte-equal; a corrupt magic, version, data offset alignment or truncated
//! payload is each rejected with `Io { op: "amb1" }`. Proves e.4.

use amoru_kernel::amb1::{Header, MAGIC, MAX_NDIM, VERSION, record_len};
use amoru_kernel::{AmoruError, DType};

const PAGE: u64 = 4096;

/// The reference writer: a whole `AMB1` record as bytes, header, padding, payload, tail padding.
fn write_record(dtype: DType, shape: &[i64], fill: u8) -> (Vec<u8>, Header) {
    let data_offset = Header::data_offset_for(shape.len(), PAGE).expect("a data offset");
    let header = Header {
        dtype,
        shape: shape.to_vec(),
        data_offset,
    };
    let total = record_len(shape.len(), header.payload_len(), PAGE).expect("a record length");
    let mut bytes = vec![0u8; total as usize];
    header.write(&mut bytes).expect("write the header");
    let start = data_offset as usize;
    let end = start + header.payload_len() as usize;
    for (i, slot) in bytes[start..end].iter_mut().enumerate() {
        *slot = fill.wrapping_add(i as u8);
    }
    (bytes, header)
}

fn is_amb1_error(err: &AmoruError) -> bool {
    matches!(err, AmoruError::Io { op: "amb1", .. })
}

#[test]
fn ct_t8_amb1_roundtrip() {
    for dtype in DType::ALL {
        for ndim in 0..=MAX_NDIM {
            let shape: Vec<i64> = (0..ndim).map(|i| (i as i64 % 3) + 1).collect();
            let (bytes, header) = write_record(dtype, &shape, 0x5a);
            assert_eq!(&bytes[0..4], &MAGIC);
            assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION);

            let read = Header::read(&bytes).expect("read back the header");
            assert_eq!(read, header, "{dtype:?} rank {ndim}");
            assert_eq!(read.dtype, dtype);
            assert_eq!(read.shape, shape);
            assert_eq!(read.data_offset % 4096, 0);
            assert!(read.data_offset >= Header::header_end(ndim));

            let payload = read.payload(&bytes).expect("the payload");
            assert_eq!(payload.len() as u64, read.payload_len());
            let expected: Vec<u8> = (0..payload.len())
                .map(|i| 0x5au8.wrapping_add(i as u8))
                .collect();
            assert_eq!(payload, expected.as_slice(), "{dtype:?} rank {ndim}");
            assert_eq!(
                read.element_count(),
                shape.iter().map(|d| *d as u64).product::<u64>()
            );
        }
    }

    // A rank-0 tensor has one element (b).
    let (bytes, header) = write_record(DType::F64, &[], 1);
    assert_eq!(header.element_count(), 1);
    assert_eq!(header.payload_len(), 8);
    assert!(Header::read(&bytes).is_ok());

    // Corrupt magic.
    let (mut corrupt, _) = write_record(DType::I32, &[4], 0);
    corrupt[0] = b'X';
    let err = Header::read(&corrupt).expect_err("a bad magic must be rejected");
    assert!(is_amb1_error(&err), "got {err}");
    assert!(err.to_string().contains("magic"));

    // An unknown version is rejected, not skipped.
    let (mut corrupt, _) = write_record(DType::I32, &[4], 0);
    corrupt[4..6].copy_from_slice(&2u16.to_le_bytes());
    let err = Header::read(&corrupt).expect_err("an unknown version must be rejected");
    assert!(
        is_amb1_error(&err) && err.to_string().contains("version"),
        "got {err}"
    );

    // A data offset that is not a multiple of 4096.
    let (mut corrupt, header) = write_record(DType::I32, &[4], 0);
    let at = 8 + 8 * header.shape.len();
    corrupt[at..at + 8].copy_from_slice(&(PAGE + 1).to_le_bytes());
    let err = Header::read(&corrupt).expect_err("a misaligned data offset must be rejected");
    assert!(
        is_amb1_error(&err) && err.to_string().contains("4096"),
        "got {err}"
    );

    // A data offset inside the header.
    let (mut corrupt, header) = write_record(DType::I32, &[4], 0);
    let at = 8 + 8 * header.shape.len();
    corrupt[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    let err = Header::read(&corrupt).expect_err("a data offset inside the header is rejected");
    assert!(is_amb1_error(&err), "got {err}");

    // A truncated payload.
    let (bytes, header) = write_record(DType::F32, &[8], 0);
    let short = &bytes[..(header.payload_end() - 1) as usize];
    let err = Header::read(short).expect_err("a truncated payload must be rejected");
    assert!(
        is_amb1_error(&err) && err.to_string().contains("short"),
        "got {err}"
    );

    // A buffer shorter than a header at all, and an unknown dtype code.
    assert!(is_amb1_error(
        &Header::read(&[0u8; 4]).expect_err("too short")
    ));
    let (mut corrupt, _) = write_record(DType::I32, &[4], 0);
    corrupt[6] = 99;
    assert!(is_amb1_error(
        &Header::read(&corrupt).expect_err("unknown dtype")
    ));
    // and an ndim past the format's limit.
    let (mut corrupt, _) = write_record(DType::I32, &[4], 0);
    corrupt[7] = 9;
    assert!(is_amb1_error(
        &Header::read(&corrupt).expect_err("ndim past 8")
    ));
    // and a negative dimension.
    let (mut corrupt, _) = write_record(DType::I32, &[4], 0);
    corrupt[8..16].copy_from_slice(&(-1i64).to_le_bytes());
    assert!(is_amb1_error(
        &Header::read(&corrupt).expect_err("a negative dimension")
    ));

    // The writer refuses what the reader would refuse.
    let header = Header {
        dtype: DType::I32,
        shape: vec![1; 9],
        data_offset: PAGE,
    };
    let mut out = vec![0u8; PAGE as usize];
    assert!(is_amb1_error(&header.write(&mut out).expect_err("rank 9")));
    let header = Header {
        dtype: DType::I32,
        shape: vec![-1],
        data_offset: PAGE,
    };
    assert!(is_amb1_error(
        &header.write(&mut out).expect_err("a negative dimension")
    ));
    let header = Header {
        dtype: DType::I32,
        shape: vec![1],
        data_offset: 100,
    };
    assert!(is_amb1_error(
        &header
            .write(&mut out)
            .expect_err("a misaligned data offset")
    ));
    let header = Header {
        dtype: DType::I32,
        shape: vec![1],
        data_offset: PAGE,
    };
    assert!(is_amb1_error(
        &header.write(&mut [0u8; 8]).expect_err("a short buffer")
    ));
    assert!(Header::data_offset_for(1, 0).is_err());
    assert!(record_len(1, 8, 0).is_err());

    // A buffer that holds a header's first bytes but not all of its shape is refused.
    let (bytes, _) = write_record(DType::I32, &[1, 2, 3], 0);
    assert!(is_amb1_error(
        &Header::read(&bytes[..20]).expect_err("a partial header")
    ));

    // Every dtype code round trips through the format's table (e.4).
    for dtype in DType::ALL {
        assert_eq!(DType::from_code(dtype.code()), Some(dtype));
    }
    assert_eq!(DType::from_code(13), None);
}
