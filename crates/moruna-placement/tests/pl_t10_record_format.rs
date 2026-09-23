//! PL-T10 record_format (e.3): a written segment, read back from the fake's in-memory file,
//! parses by an independent reader in the test; segment numbers are global and one segment
//! holds one stage.
//!
//! 09 e.3 also asks for the IPC body to be readable by arrow's own stream reader. It is not,
//! and cannot be: `encode_framing` rewrites every buffer entry to its page-aligned place and
//! leaves the record batch message's `bodyLength` at the compact total, which is the
//! deviation contracts e.7 permits, so arrow's reader stops short of the last buffer (it
//! panics inside `arrow-buffer` rather than returning an error). The body is read here by
//! `moruna_kernel::ipc::decode`, which is the reader the format is written for. Reported as a
//! finding against 09 e.3.

mod common;

use moruna_kernel::arrow::buffer::Buffer as ArrowBuffer;
use moruna_kernel::{Locality, Placement, StagingCodec, TierKind, mrb1, ipc};
use moruna_testkit::{FakeAllocator, FakeReactor};

/// The header page of e.3, parsed by hand so the test does not borrow the engine's reader.
struct Header {
    stage: u16,
    kind: u8,
    codec: u8,
    payload_len: u64,
    body_offset: u64,
}

fn parse(page: &[u8]) -> Header {
    assert_eq!(&page[0..8], b"MORUNSEG", "magic at offset 0");
    assert_eq!(&page[20..24], &[0u8; 4], "reserved bytes are zero");
    assert_eq!(&page[40..64], &[0u8; 24], "reserved bytes are zero");
    let eight = |at: usize| u64::from_le_bytes(page[at..at + 8].try_into().expect("8 bytes"));
    assert!(
        eight(8) < u64::MAX,
        "the sequence number is in the header (e.3)"
    );
    Header {
        stage: u16::from_le_bytes([page[16], page[17]]),
        kind: page[18],
        codec: page[19],
        payload_len: eight(24),
        body_offset: eight(32),
    }
}

fn run_dir(scratch: &common::Scratch) -> std::path::PathBuf {
    scratch.path().join(format!("moruna-{}", "07".repeat(16)))
}

#[test]
fn pl_t10_record_format() {
    let page = common::PAGE as u64;
    let scratch = common::Scratch::new("t10");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.segment_bytes = 64 * 1024;
    let engine = common::engine(cfg, &alloc, &reactor);
    for stage in 0..2u16 {
        engine.set_staging(stage, true);
        engine.set_promotion_window(stage, 1);
    }
    let table = common::table_morsel(&alloc, 0, 0, 256);
    let bytes = table.bytes;
    drop(table);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    engine.set_water(1, TierKind::Host, bytes, bytes);

    // Two queues writing alternately: the segment counter is one for the engine (e.3).
    for seq in 0..6u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 256))
            .expect("push");
        engine
            .push(1, common::tensor_morsel(&alloc, 100 + seq, 1, 1024))
            .expect("push");
        common::settle(&reactor);
    }
    common::settle(&reactor);

    let dir = run_dir(&scratch);
    let mut numbers: Vec<u32> = Vec::new();
    for n in 0..16u32 {
        let path = dir.join(format!("seg-{n:06}.seg"));
        if reactor.file(&path.display().to_string()).is_some() {
            numbers.push(n);
        }
    }
    assert!(
        numbers.len() >= 2,
        "both queues rolled at least one segment"
    );
    assert_eq!(
        numbers,
        (0..numbers.len() as u32).collect::<Vec<_>>(),
        "segment numbers are global and consecutive (e.3)"
    );

    let mut seen_table = 0usize;
    let mut seen_tensor = 0usize;
    for number in &numbers {
        let path = dir.join(format!("seg-{number:06}.seg"));
        let file = reactor
            .file(&path.display().to_string())
            .expect("the segment's bytes");
        let mut at = 0u64;
        let mut stage_of_segment: Option<u16> = None;
        while (at + page) as usize <= file.len() && file[at as usize..at as usize + 8] != [0u8; 8] {
            let header = parse(&file[at as usize..(at + page) as usize]);
            assert_eq!(
                header.body_offset,
                at + page,
                "the body begins one page after the header (e.3)"
            );
            assert_eq!(header.codec, StagingCodec::Raw.code(), "codec Raw in v1");
            match stage_of_segment {
                None => stage_of_segment = Some(header.stage),
                Some(stage) => assert_eq!(
                    stage, header.stage,
                    "one segment holds records of one stage (e.3)"
                ),
            }
            let body = &file
                [header.body_offset as usize..(header.body_offset + header.payload_len) as usize];
            match header.kind {
                0 => {
                    let batch = ipc::decode(ArrowBuffer::from(body.to_vec()), common::PAGE)
                        .expect("the IPC body decodes");
                    assert_eq!(batch.num_rows(), 256);
                    seen_table += 1;
                }
                1 => {
                    let parsed = mrb1::Header::read(body).expect("the MRB1 body parses");
                    assert_eq!(parsed.shape, vec![1024]);
                    assert_eq!(
                        parsed.data_offset % page,
                        0,
                        "the tensor bytes start on a page boundary"
                    );
                    seen_tensor += 1;
                }
                other => panic!("unknown record kind {other}"),
            }
            // The next record begins at the next page boundary after this one (e.3).
            at = at + page + header.payload_len.div_ceil(page) * page;
        }
    }
    assert!(seen_table > 0 && seen_tensor > 0, "both payload kinds");

    // And every record still reads back through the engine, in order.
    engine.close(0);
    let mut order = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        order.push(morsel.seq);
    }
    assert_eq!(order, (0..6u64).collect::<Vec<_>>());
}

#[test]
fn pl_t10_a_record_that_is_not_one_is_refused() {
    // The reader of e.3 refuses rather than guesses: a read that did not reach the header
    // page, a body shorter than the record declares, a page size that does not fit, and a
    // body that is not the payload the header names.
    use moruna_kernel::PayloadKind;
    use moruna_placement::staging::read::decode_record;
    use moruna_placement::staging::segment::RecordHeader;
    let alloc = FakeAllocator::new();
    let page = common::PAGE as u64;

    let short = alloc.buffer(64, alloc.host_tier());
    assert!(
        decode_record(short, page).is_err(),
        "a read short of the header page is refused"
    );
    let any = alloc.buffer(common::PAGE, alloc.host_tier());
    assert!(
        decode_record(any, u64::from(u32::MAX) + 1).is_err(),
        "a page size that does not fit this platform is refused"
    );

    let mut record = alloc.buffer(2 * common::PAGE, alloc.host_tier());
    RecordHeader {
        seq: 5,
        stage: 0,
        kind: PayloadKind::Table,
        codec: StagingCodec::Raw,
        payload_len: 8 * page,
        body_offset: page,
    }
    .write(&mut record[..common::PAGE])
    .expect("header");
    assert!(
        decode_record(record, page).is_err(),
        "a body shorter than the record declares is refused"
    );

    let mut tensor = alloc.buffer(2 * common::PAGE, alloc.host_tier());
    RecordHeader {
        seq: 6,
        stage: 0,
        kind: PayloadKind::Tensor,
        codec: StagingCodec::Raw,
        payload_len: page,
        body_offset: page,
    }
    .write(&mut tensor[..common::PAGE])
    .expect("header");
    assert!(
        decode_record(tensor, page).is_err(),
        "a body that is not an MRB1 record is refused"
    );
}
