//! Garbage from the host: each thing no well-behaved host sends is refused by name,
//! without a panic, and the card goes on working after it.

use std::{cell::RefCell, rc::Rc};

use moruna_netvsc::NetvscError;
use netvsp_protocol::{
    protocol::{self, Status},
    rndisprot,
};
use vmbus_ring::TransferPageRange;
use zerocopy::IntoBytes;

use crate::fake::{
    Card, Completes, Fate, Got, Host, PAGE, RECEIVE_SET, ROOMY, SMALL, Settings, Step,
    VF_TRANSACTION, assert_clean, assert_malformed, down, drain, on, poll, put32, rndis_frame,
    rndis_message, start_on, start_with, started, up,
};

const RNDIS: u32 = protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET;
const COMPLETE: u32 = protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE;
/// Bytes of a receive section for standard Ethernet, as netvsp cuts them.
const SECTION: usize = 1806;
/// A receive section the host is not using, for what it should never have written.
const SPARE: usize = 100;
/// A transaction the host never lent anything under.
const STRAY: u64 = 1000;

/// A started card whose host is about to misbehave, and so judges nothing.
fn lenient() -> (Card, Rc<RefCell<Host>>) {
    let (card, host) = started(ROOMY);
    host.borrow_mut().lenient = true;
    assert_eq!(host.borrow().section(), SECTION);
    (card, host)
}

/// The card carries a frame each way.
fn works(card: &mut Card, host: &Rc<RefCell<Host>>) {
    let out = vec![0xa5; 333];
    let back = vec![0x5a; 777];
    assert!(card.send(&out).unwrap());
    host.borrow_mut().inject(&back);
    assert_eq!(drain(card), [Got::Frame(back)]);
    assert_eq!(host.borrow().frames.last(), Some(&out));
}

/// What `garbage` has the host send is refused, the refusal saying `part`; nothing
/// follows it, and the card goes on working.
#[track_caller]
fn refused(garbage: impl FnOnce(&mut Host), part: &str) {
    let (mut card, host) = lenient();
    garbage(&mut host.borrow_mut());
    let e = poll(&mut card).unwrap_err();
    assert_malformed(&e, part);
    assert!(e.to_string().starts_with("the host sent "), "{e}");
    assert_eq!(poll(&mut card).unwrap(), None);
    works(&mut card, &host);
}

fn range(at: usize, count: usize) -> TransferPageRange {
    TransferPageRange {
        byte_count: count as u32,
        byte_offset: at as u32,
    }
}

/// A receive of `ranges`, well formed but for them.
fn receive(host: &mut Host, ranges: Vec<TransferPageRange>) {
    let payload = host.rndis_packet(protocol::DATA_CHANNEL_TYPE);
    host.raw_receive(STRAY, RECEIVE_SET, payload, ranges);
}

/// A receive of `count` bytes of the spare section, which hold `message`.
fn receive_of(host: &mut Host, message: &[u8], count: usize) {
    host.scribble(SPARE * SECTION, message);
    receive(host, vec![range(SPARE * SECTION, count)]);
}

/// A frame as the host lays it out, with a field of it rewritten.
fn frame_with(field: usize, value: u32) -> Vec<u8> {
    let mut message = rndis_frame(&[7; 100], SECTION);
    put32(&mut message, field, value);
    message
}

/// The two fields a frame's message places its data with.
const DATA_OFFSET: usize = 8;
const DATA_LENGTH: usize = 12;

#[test]
fn a_completion_of_nothing_in_flight_is_refused() {
    for transaction in [0, 5, 99, 1 << 32, u64::MAX] {
        refused(
            |host| {
                let status = Status::SUCCESS;
                let payload = host.nvsp(
                    COMPLETE,
                    &protocol::Message1SendRndisPacketComplete { status },
                );
                host.raw_completion(transaction, payload);
            },
            &format!("a completion of transaction {transaction:#x}, which is not in flight"),
        );
    }
}

#[test]
fn a_completion_that_says_something_else_frees_its_section_and_is_refused() {
    // Two send sections. The host answers the first frame with what is no completion
    // of a frame: another message, and nothing at all. (A packet's payload is whole
    // eight-byte words, so a completion cannot be cut off inside its status.)
    let bad: [(Vec<u8>, &str); 2] = [
        (
            [protocol::MESSAGE_TYPE_INIT_COMPLETE.as_bytes(), &[0; 28]].concat(),
            "NVSP message 2 for a frame sent, where message 108 is due",
        ),
        (
            Vec::new(),
            "0 bytes for a frame sent, too few for an NVSP message",
        ),
    ];
    for (payload, part) in bad {
        let (mut card, host) = started(SMALL);
        host.borrow_mut().lenient = true;
        host.borrow_mut().hold = true;
        assert!(card.send(&[1; 60]).unwrap());
        // The frame went in section 0, and its transaction is its section's, 1.
        host.borrow_mut().raw_completion(1, payload);
        let e = poll(&mut card).unwrap_err();
        assert_malformed(&e, part);
        // The host has answered the transaction, so the section is free: both are.
        host.borrow_mut().forget_held();
        assert!(card.send(&[2; 60]).unwrap());
        assert!(card.send(&[3; 60]).unwrap());
        assert!(!card.send(&[4; 60]).unwrap());
    }
}

#[test]
fn a_message_the_card_never_asked_for_is_refused() {
    // The table of sub-channels to send on: only a guest that asked for sub-channels
    // is sent one.
    refused(
        |host| {
            let table = protocol::Message5SendIndirectionTable {
                table_entry_count: 16,
                table_offset: 12,
            };
            let payload = host.nvsp(protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE, &table);
            host.raw_message(None, payload);
        },
        "NVSP message 134 for a message of its own, where message 128 is due",
    );
    // That the data path is the synthetic one again: only a guest that moved it is told.
    refused(
        |host| {
            let path = protocol::Message4SwitchDataPath {
                active_data_path: protocol::DataPath::SYNTHETIC.0,
            };
            let payload = host.nvsp(protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH, &path);
            host.raw_message(Some(0x8000_0000_0000_0001), payload);
        },
        "NVSP message 129 for a message of its own, where message 128 is due",
    );
    refused(
        |host| host.raw_message(Some(7), Vec::new()),
        "0 bytes for a message of its own, too few for an NVSP message",
    );
    refused(
        |host| host.raw_direct(5, vec![0; 40]),
        "a packet naming guest pages, which only a guest sends",
    );
}

#[test]
fn a_cut_off_virtual_function_message_is_refused_and_not_acknowledged() {
    let (mut card, host) = lenient();
    let cut = [
        protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION.as_bytes(),
        1u32.as_bytes(),
    ]
    .concat();
    host.borrow_mut().raw_message(Some(VF_TRANSACTION), cut);
    let e = poll(&mut card).unwrap_err();
    assert_malformed(
        &e,
        "NVSP message 128 for a message of its own in 8 bytes, too few",
    );
    assert_eq!(poll(&mut card).unwrap(), None);
    assert_eq!(host.borrow().vf_acknowledged, 0);
    works(&mut card, &host);
}

#[test]
fn a_receive_that_is_not_one_is_refused() {
    refused(
        |host| {
            let payload = host.rndis_packet(protocol::DATA_CHANNEL_TYPE);
            host.raw_receive(STRAY, 7, payload, vec![range(SPARE * SECTION, SECTION)]);
        },
        "a receive from transfer page set 7, which is not the receive buffer",
    );
    refused(
        |host| {
            let complete = protocol::Message1SendRndisPacketComplete {
                status: Status::SUCCESS,
            };
            let payload = host.nvsp(COMPLETE, &complete);
            host.raw_receive(
                STRAY,
                RECEIVE_SET,
                payload,
                vec![range(SPARE * SECTION, SECTION)],
            );
        },
        "NVSP message 108 for a receive, where message 107 is due",
    );
    refused(
        |host| {
            host.raw_receive(
                STRAY,
                RECEIVE_SET,
                Vec::new(),
                vec![range(SPARE * SECTION, SECTION)],
            )
        },
        "0 bytes for a receive, too few for an NVSP message",
    );
    refused(
        |host| receive(host, Vec::new()),
        "a receive that names 0 ranges",
    );
    // One more range than any host sends in a receive.
    refused(
        |host| receive(host, vec![range(SPARE * SECTION, SECTION); 376]),
        "a receive that names 376 ranges",
    );
}

#[test]
fn a_receive_whose_descriptor_lies_is_refused() {
    const TRANSFER_PAGES: u16 = 7;
    const COMPLETION_REQUESTED: u16 = 1;
    // Five ranges claimed, in a descriptor with room for none: the payload starts
    // right after the transfer header, three words in.
    refused(
        |host| {
            let rest = [
                RECEIVE_SET.as_bytes(),
                0u16.as_bytes(),
                5u32.as_bytes(),
                &host.rndis_packet(protocol::DATA_CHANNEL_TYPE),
            ]
            .concat();
            host.raw_descriptor(TRANSFER_PAGES, 3, COMPLETION_REQUESTED, STRAY, &rest);
        },
        "a receive that names 5 ranges",
    );
    // A receive that asks for no completion could never give its section back.
    refused(
        |host| {
            let rest = [
                RECEIVE_SET.as_bytes(),
                0u16.as_bytes(),
                1u32.as_bytes(),
                range(SPARE * SECTION, SECTION).as_bytes(),
                &host.rndis_packet(protocol::DATA_CHANNEL_TYPE),
            ]
            .concat();
            host.raw_descriptor(TRANSFER_PAGES, 4, 0, STRAY, &rest);
        },
        "a receive that asks for no completion",
    );
}

#[test]
fn a_range_outside_the_receive_sections_is_refused() {
    let buffer = 64 * PAGE;
    // 145 sections of 1,806 bytes end 274 bytes before the buffer does.
    let end = 145 * SECTION;
    for (at, count) in [
        (buffer - 100, 200),
        (buffer, 8),
        (end, 100),
        (end - 8, 9),
        (u32::MAX as usize, u32::MAX as usize),
        (0, u32::MAX as usize),
    ] {
        refused(
            |host| receive(host, vec![range(at, count)]),
            &format!("a receive of {count} bytes at byte {at}, with sections from byte 0 to {end}"),
        );
    }
    // Too short to hold an RNDIS message's type and length.
    refused(
        |host| receive(host, vec![range(SPARE * SECTION, 4)]),
        "a receive of 4 bytes at byte 180600",
    );
}

#[test]
fn an_rndis_message_that_overruns_its_range_is_refused() {
    let header = |message_type, message_length| {
        rndisprot::MessageHeader {
            message_type,
            message_length,
        }
        .as_bytes()
        .to_vec()
    };
    refused(
        |host| receive_of(host, &header(rndisprot::MESSAGE_TYPE_PACKET_MSG, 5000), 100),
        "an RNDIS message of 5000 bytes in a receive of 100 bytes",
    );
    refused(
        |host| receive_of(host, &header(rndisprot::MESSAGE_TYPE_PACKET_MSG, 4), 100),
        "an RNDIS message of 4 bytes in a receive of 100 bytes",
    );
    refused(
        |host| receive_of(host, &header(rndisprot::MESSAGE_TYPE_PACKET_MSG, 20), 100),
        "an RNDIS packet message of 20 bytes",
    );
    refused(
        |host| {
            receive_of(
                host,
                &header(rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG, 12),
                100,
            )
        },
        "an RNDIS indication of 12 bytes",
    );
    refused(
        |host| receive_of(host, &header(rndisprot::MESSAGE_TYPE_SET_CMPLT, 12), 100),
        "an RNDIS completion of 12 bytes",
    );
    // A completion longer than a section, in a range of two sections.
    refused(
        |host| {
            receive_of(
                host,
                &header(rndisprot::MESSAGE_TYPE_QUERY_CMPLT, 2 * SECTION as u32),
                2 * SECTION,
            )
        },
        "an RNDIS completion of 3612 bytes",
    );
}

#[test]
fn an_rndis_message_the_card_has_no_use_for_is_refused() {
    for (message_type, name) in [
        (rndisprot::MESSAGE_TYPE_KEEPALIVE_MSG, "0x8"),
        (rndisprot::MESSAGE_TYPE_KEEPALIVE_CMPLT, "0x80000008"),
        (rndisprot::MESSAGE_TYPE_RESET_CMPLT, "0x80000006"),
        (rndisprot::MESSAGE_TYPE_HALT_MSG, "0x3"),
        (0, "0x0"),
        (0xffff_ffff, "0xffffffff"),
    ] {
        refused(
            |host| {
                let message = rndis_message(
                    message_type,
                    &rndisprot::KeepaliveRequest { request_id: 1 },
                    &[],
                );
                receive_of(host, &message, SECTION);
            },
            &format!("RNDIS message {name}, which this driver has no use for"),
        );
    }
}

#[test]
fn a_frame_that_does_not_lie_in_its_message_is_refused() {
    // The data runs past the end of the message.
    refused(
        |host| receive_of(host, &frame_with(DATA_LENGTH, SECTION as u32), SECTION),
        "a frame of 1806 bytes at byte 248 of a message of 1806 bytes",
    );
    refused(
        |host| receive_of(host, &frame_with(DATA_LENGTH, u32::MAX), SECTION),
        "a frame of 4294967295 bytes at byte 248",
    );
    refused(
        |host| receive_of(host, &frame_with(DATA_OFFSET, u32::MAX), SECTION),
        "a frame of 100 bytes at byte 4294967295",
    );
    // The data starts inside the message's own fields.
    refused(
        |host| receive_of(host, &frame_with(DATA_OFFSET, 8), SECTION),
        "a frame of 100 bytes at byte 8 of a message of 1806 bytes",
    );
}

#[test]
fn a_frame_of_no_ethernet_length_is_refused() {
    for len in [0, 1, 13, 1515, 1550] {
        refused(
            |host| receive_of(host, &rndis_frame(&vec![7; len], SECTION), SECTION),
            &format!("a frame of {len} bytes"),
        );
    }
    // The shortest and the longest frame are taken, from the same place.
    for len in [14, 1514] {
        let (mut card, host) = lenient();
        receive_of(
            &mut host.borrow_mut(),
            &rndis_frame(&vec![7; len], SECTION),
            SECTION,
        );
        assert_eq!(drain(&mut card), [Got::Frame(vec![7; len])]);
    }
}

#[test]
fn an_rndis_completion_nobody_asked_for_is_refused() {
    let (mut card, host) = started(ROOMY);
    for message_type in [
        rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT,
        rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
        rndisprot::MESSAGE_TYPE_SET_CMPLT,
    ] {
        host.borrow_mut().control(rndis_message(
            message_type,
            &rndisprot::SetComplete {
                request_id: 9,
                status: rndisprot::STATUS_SUCCESS,
            },
            &[],
        ));
        let e = poll(&mut card).unwrap_err();
        assert_malformed(&e, "an RNDIS completion of a request nobody made");
        assert_eq!(poll(&mut card).unwrap(), None);
    }
    // The host lent a section for each, and has each back, as a success: the receive
    // itself was a good one.
    assert_eq!(host.borrow().free_control_sections(), 16);
    assert_clean(&host);
    works(&mut card, &host);
}

#[test]
fn a_bad_range_among_good_ones_fails_the_receive_and_spares_the_rest() {
    let (mut card, host) = started(ROOMY);
    let frames = [vec![1; 60], vec![2; 60], vec![3; 60]];
    let all: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    host.borrow_mut().deliver(&all);
    // The host's first frame section is the seventeenth. It spoils the second frame's
    // message: its length is now four bytes.
    host.borrow_mut()
        .scribble(17 * SECTION + 4, 4u32.as_bytes());
    assert_eq!(poll(&mut card).unwrap(), Some(Got::Frame(vec![1; 60])));
    let e = poll(&mut card).unwrap_err();
    assert_malformed(&e, "an RNDIS message of 4 bytes in a receive of 1806 bytes");
    assert_eq!(poll(&mut card).unwrap(), Some(Got::Frame(vec![3; 60])));
    assert_eq!(poll(&mut card).unwrap(), None);
    // All three sections went back, and the host was told the receive failed.
    assert_eq!(host.borrow().free_sections(), 129);
    assert_eq!(host.borrow().received.last(), Some(&Status::FAILURE));
    assert_clean(&host);
    works(&mut card, &host);
}

#[test]
fn a_host_that_reads_no_completion_is_refused_once_too_many_are_owed() {
    // The ring to the host is one page: 63 completions. 145 receive sections and eight
    // acknowledgements more may be owed; the next packet to complete is refused.
    let (card, host) = start_on(PAGE, 16 * PAGE, ROOMY, |_| ());
    let mut card = up(card);
    {
        let mut host = host.borrow_mut();
        host.lenient = true;
        host.paused = true;
        host.scribble(SPARE * SECTION, &rndis_frame(&[8; 64], SECTION));
        for n in 0..230 {
            let payload = host.rndis_packet(protocol::DATA_CHANNEL_TYPE);
            host.raw_receive(
                STRAY + n,
                RECEIVE_SET,
                payload,
                vec![range(SPARE * SECTION, SECTION)],
            );
        }
    }
    let mut frames = 0;
    let e = loop {
        match poll(&mut card) {
            Ok(Some(Got::Frame(frame))) => {
                assert_eq!(frame, [8; 64]);
                frames += 1;
            }
            Ok(other) => panic!("{other:?} after {frames} frames"),
            Err(e) => break e,
        }
    };
    assert_eq!(frames, 63 + 153 + 1);
    assert_malformed(
        &e,
        "one more packet to complete with 153 completions unread",
    );
    // The host reads again, and the card carries on: the rest of the frames, then
    // frames each way.
    host.borrow_mut().paused = false;
    card.run_host();
    assert_eq!(drain(&mut card).len(), 230 - frames);
    works(&mut card, &host);
}

#[test]
fn a_ring_that_is_no_ring_is_an_error_every_time() {
    // The write pointer of the ring to the guest points nowhere.
    let (mut card, host) = lenient();
    host.borrow_mut().corrupt_ring();
    for _ in 0..3 {
        let e = poll(&mut card).unwrap_err();
        assert!(matches!(e, NetvscError::Ring(_)), "{e}");
        assert_eq!(e.to_string(), "the ring: invalid ring buffer pointer");
    }

    // A packet of a type no ring carries.
    let (mut card, host) = lenient();
    host.borrow_mut().raw_descriptor(99, 2, 0, 0, &[0; 8]);
    for _ in 0..3 {
        let e = poll(&mut card).unwrap_err();
        assert!(matches!(e, NetvscError::Ring(_)), "{e}");
        assert_eq!(e.to_string(), "the ring: unknown packet descriptor type");
    }

    // The read pointer of the ring to the host, which is the host's to write, points
    // nowhere: nothing can be sent, and a frame received cannot be completed.
    let (mut card, host) = lenient();
    host.borrow_mut().corrupt_ring_to_host();
    let e = card.send(&[1; 60]).unwrap_err();
    assert!(matches!(e, NetvscError::Ring(_)), "{e}");
    host.borrow_mut().inject(&[2; 60]);
    assert_eq!(poll(&mut card).unwrap(), Some(Got::Frame(vec![2; 60])));
    let e = poll(&mut card).unwrap_err();
    assert!(matches!(e, NetvscError::Ring(_)), "{e}");
}

// Initialisation.

/// Why a card whose host `prepare` set up did not start.
fn start_refused(prepare: impl FnOnce(&mut Host)) -> NetvscError {
    let (card, _host) = start_with(SMALL, prepare);
    down(card)
}

/// Rewrites the host's `nth` completion of the guest's NVSP message `message_type`.
fn answer(
    message_type: u32,
    nth: usize,
    mut change: impl FnMut(&mut Vec<u8>) + 'static,
) -> impl FnOnce(&mut Host) {
    move |host| {
        host.mangle = on(Step::Answer(message_type), nth, move |_, bytes| {
            change(bytes);
            Fate::Send
        });
    }
}

/// Rewrites the host's `nth` RNDIS message of type `message_type`.
fn rndis(
    message_type: u32,
    nth: usize,
    mut change: impl FnMut(&mut Vec<u8>) + 'static,
) -> impl FnOnce(&mut Host) {
    move |host| {
        host.mangle = on(Step::Rndis(message_type), nth, move |_, bytes| {
            change(bytes);
            Fate::Send
        });
    }
}

/// Makes an RNDIS message `len` bytes long, and say so.
fn resize(bytes: &mut Vec<u8>, len: usize) {
    bytes.resize(len, 0);
    put32(bytes, 4, len as u32);
}

/// The fields of the host's answers, by where they lie.
const NVSP_TYPE: usize = 0;
const NVSP_STATUS: usize = 4;
const RECEIVE_LISTS: usize = 8;
const RECEIVE_START: usize = 12;
const RECEIVE_SECTION: usize = 16;
const RECEIVE_COUNT: usize = 20;
const SEND_SECTION: usize = 8;
const RNDIS_TYPE: usize = 0;
const RNDIS_LENGTH: usize = 4;
const RNDIS_REQUEST: usize = 8;
const RNDIS_STATUS: usize = 12;
const VALUE_LENGTH: usize = 16;
const VALUE_OFFSET: usize = 20;
const VALUE: usize = 24;
const MEDIUM: usize = 28;

#[test]
fn an_answer_to_the_version_that_is_none_is_refused() {
    let init = protocol::MESSAGE_TYPE_INIT;
    let e = start_refused(answer(init, 0, |b| put32(b, NVSP_TYPE, 999)));
    assert_malformed(
        &e,
        "NVSP message 999 for the NVSP version, where message 2 is due",
    );
    let e = start_refused(answer(init, 0, |b| b.clear()));
    assert_malformed(
        &e,
        "0 bytes for the NVSP version, too few for an NVSP message",
    );
    let e = start_refused(answer(init, 0, |b| b.truncate(8)));
    assert_malformed(
        &e,
        "NVSP message 2 for the NVSP version in 8 bytes, too few",
    );
}

#[test]
fn another_packet_where_an_answer_is_due_is_refused() {
    // Something is on the ring before the card has said a word.
    let e = start_refused(|host| host.raw_message(None, vec![0; 8]));
    assert_malformed(
        &e,
        "another packet where its answer to the NVSP version is due",
    );
    // The host says something of its own before it answers the NDIS configuration.
    let config = protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG;
    let e = start_refused(|host| {
        host.mangle = on(Step::Answer(config), 0, |host, _| {
            host.offer_vf(1);
            Fate::Send
        });
    });
    assert_malformed(
        &e,
        "another packet where its answer to the NDIS configuration is due",
    );
    // The host completes a transaction that is not the NDIS version's.
    let version = protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION;
    let e = start_refused(|host| {
        host.mangle = on(Step::Answer(version), 0, |host, _| {
            host.raw_completion(12345, Vec::new());
            Fate::Send
        });
    });
    assert_malformed(
        &e,
        "another packet where its answer to the NDIS version is due",
    );
}

#[test]
fn receive_sections_that_do_not_lie_in_the_buffer_are_refused() {
    let buffer = protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER;
    let e = start_refused(answer(buffer, 0, |b| put32(b, NVSP_STATUS, 2)));
    assert!(
        matches!(
            e,
            NetvscError::Refused {
                step: "the receive buffer",
                status: 2,
                ..
            }
        ),
        "{e}"
    );
    let e = start_refused(answer(buffer, 0, |b| put32(b, NVSP_TYPE, 105)));
    assert_malformed(
        &e,
        "NVSP message 105 for the receive buffer, where message 102 is due",
    );
    let e = start_refused(answer(buffer, 0, |b| b.truncate(24)));
    assert_malformed(
        &e,
        "NVSP message 102 for the receive buffer in 24 bytes, too few",
    );
    for lists in [0, 2] {
        let e = start_refused(answer(buffer, 0, move |b| put32(b, RECEIVE_LISTS, lists)));
        assert_malformed(&e, &format!("{lists} lists of receive sections"));
    }
    // Eight pages: 32,768 bytes.
    let cases: [(usize, u32, &str); 8] = [
        (
            RECEIVE_SECTION,
            0,
            "18 receive sections of 0 bytes from byte 0",
        ),
        // Too short for the shortest frame behind its RNDIS fields.
        (
            RECEIVE_SECTION,
            57,
            "18 receive sections of 57 bytes from byte 0",
        ),
        (RECEIVE_SECTION, 1821, "18 receive sections of 1821 bytes"),
        (
            RECEIVE_SECTION,
            u32::MAX,
            "18 receive sections of 4294967295 bytes",
        ),
        (RECEIVE_COUNT, 0, "0 receive sections of 1806 bytes"),
        (RECEIVE_COUNT, 19, "19 receive sections of 1806 bytes"),
        (
            RECEIVE_COUNT,
            u32::MAX,
            "4294967295 receive sections of 1806 bytes",
        ),
        (
            RECEIVE_START,
            261,
            "18 receive sections of 1806 bytes from byte 261",
        ),
    ];
    for (field, value, part) in cases {
        let e = start_refused(answer(buffer, 0, move |b| put32(b, field, value)));
        assert_malformed(&e, part);
        assert_malformed(&e, "in a buffer of 32768 bytes");
    }
    // Sections of 58 bytes hold the shortest frame, and are taken as sections. The MTU
    // the host states next cannot be delivered in them, and that is refused.
    let e = start_refused(answer(buffer, 0, |b| put32(b, RECEIVE_SECTION, 58)));
    assert_malformed(&e, "an MTU of 1500, with receive sections of 58 bytes");
}

#[test]
fn a_receive_before_the_sections_the_host_stated_is_refused() {
    // The host states sections from byte 4,096, and then answers in its own first
    // section, at byte 0.
    let buffer = protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER;
    let e = start_refused(answer(buffer, 0, |b| {
        put32(b, RECEIVE_START, 4096);
        put32(b, RECEIVE_COUNT, 15);
    }));
    assert_malformed(
        &e,
        "a receive of 52 bytes at byte 0, with sections from byte 4096 to 31186",
    );
}

#[test]
fn send_sections_that_cannot_be_used_are_refused() {
    let buffer = protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER;
    let e = start_refused(answer(buffer, 0, |b| put32(b, NVSP_STATUS, 2)));
    assert!(
        matches!(
            e,
            NetvscError::Refused {
                step: "the send buffer",
                status: 2,
                ..
            }
        ),
        "{e}"
    );
    let e = start_refused(answer(buffer, 0, |b| b.truncate(8)));
    assert_malformed(
        &e,
        "NVSP message 105 for the send buffer in 8 bytes, too few",
    );
    // No section, a section too short for a control message, a section longer than the
    // buffer.
    for size in [0, 31, 4 * PAGE as u32 + 1, u32::MAX] {
        let e = start_refused(answer(buffer, 0, move |b| put32(b, SEND_SECTION, size)));
        assert_malformed(
            &e,
            &format!("send sections of {size} bytes, in a buffer of 16384 bytes"),
        );
    }
    // A section of 32 bytes holds each control message, and RNDIS initialises, but it
    // holds no frame of the MTU: the card does not start.
    let (card, host) = start_with(SMALL, |host| host.send_section = 32);
    let e = down(card);
    assert!(
        matches!(
            e,
            NetvscError::Unfit {
                message: 1558,
                section: 32
            }
        ),
        "{e}"
    );
    assert_clean(&host);
}

#[test]
fn an_rndis_request_the_host_refuses_or_fails_ends_the_start() {
    // The host could not take the packet the request came in.
    let e = start_refused(answer(RNDIS, 0, |b| put32(b, NVSP_STATUS, 2)));
    assert!(matches!(e, NetvscError::NotSent(2)), "{e}");
    // The host answers each request in turn with a failure.
    let cases = [
        (
            rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT,
            0,
            rndisprot::STATUS_FAILURE,
            "RNDIS initialisation",
        ),
        (
            rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
            0,
            rndisprot::STATUS_NOT_SUPPORTED,
            "the query of its address",
        ),
        (
            rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
            1,
            rndisprot::STATUS_NOT_SUPPORTED,
            "the query of its MTU",
        ),
        (
            rndisprot::MESSAGE_TYPE_SET_CMPLT,
            0,
            rndisprot::STATUS_INVALID_DATA,
            "the packet filter",
        ),
    ];
    for (message_type, nth, status, step) in cases {
        let e = start_refused(rndis(message_type, nth, move |b| {
            put32(b, RNDIS_STATUS, status)
        }));
        match e {
            NetvscError::Refused {
                step: s, status: n, ..
            } if s == step && n == status => {}
            other => panic!("{step}: {other}"),
        }
    }
    let e = start_refused(rndis(rndisprot::MESSAGE_TYPE_SET_CMPLT, 0, |b| {
        put32(b, RNDIS_STATUS, rndisprot::STATUS_INVALID_DATA)
    }));
    assert_eq!(
        e.to_string(),
        "the host refused the packet filter with status 0xc0010015 (NVSP 6.1, NDIS 6.30)"
    );
}

#[test]
fn an_answer_to_another_rndis_request_is_refused() {
    let initialize = rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT;
    let due = "another RNDIS completion where its answer to RNDIS initialisation is due";
    let e = start_refused(rndis(initialize, 0, |b| put32(b, RNDIS_REQUEST, 77)));
    assert_malformed(&e, due);
    let e = start_refused(rndis(initialize, 0, |b| {
        put32(b, RNDIS_TYPE, rndisprot::MESSAGE_TYPE_QUERY_CMPLT)
    }));
    assert_malformed(&e, due);
    // An answer to a query with the right ID and the wrong type.
    let e = start_refused(rndis(rndisprot::MESSAGE_TYPE_QUERY_CMPLT, 1, |b| {
        put32(b, RNDIS_TYPE, rndisprot::MESSAGE_TYPE_SET_CMPLT)
    }));
    assert_malformed(
        &e,
        "another RNDIS completion where its answer to the query of its MTU is due",
    );
}

#[test]
fn an_answer_to_rndis_initialisation_that_is_cut_off_or_not_ethernet_is_refused() {
    let initialize = rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT;
    let e = start_refused(rndis(initialize, 0, |b| resize(b, 20)));
    assert_malformed(&e, "20 bytes in answer to RNDIS initialisation");
    let e = start_refused(rndis(initialize, 0, |b| resize(b, 12)));
    assert_malformed(&e, "an RNDIS completion of 12 bytes");
    let e = start_refused(rndis(initialize, 0, |b| put32(b, RNDIS_LENGTH, 4)));
    assert_malformed(&e, "an RNDIS message of 4 bytes in a receive of 52 bytes");
    let e = start_refused(rndis(initialize, 0, |b| put32(b, RNDIS_LENGTH, 5000)));
    assert_malformed(
        &e,
        "an RNDIS message of 5000 bytes in a receive of 52 bytes",
    );
    let e = start_refused(rndis(initialize, 0, |b| {
        put32(b, MEDIUM, rndisprot::MEDIUM_802_5)
    }));
    assert_malformed(
        &e,
        "medium 1 in answer to RNDIS initialisation, which is not Ethernet",
    );
}

#[test]
fn an_address_that_is_not_six_bytes_in_its_answer_is_refused() {
    let query = rndisprot::MESSAGE_TYPE_QUERY_CMPLT;
    let e = start_refused(rndis(query, 0, |b| {
        resize(b, 28);
        put32(b, VALUE_LENGTH, 4);
    }));
    assert_malformed(&e, "an address of 4 bytes");
    let e = start_refused(rndis(query, 0, |b| {
        resize(b, 32);
        put32(b, VALUE_LENGTH, 8);
    }));
    assert_malformed(&e, "an address of 8 bytes");
    let e = start_refused(rndis(query, 0, |b| put32(b, VALUE_LENGTH, 0)));
    assert_malformed(&e, "an address of 0 bytes");
    // The value lies in the answer's own fields, or outside the answer.
    let e = start_refused(rndis(query, 0, |b| put32(b, VALUE_OFFSET, 4)));
    assert_malformed(
        &e,
        "a value of 6 bytes at byte 4 in an answer of 30 bytes to the query of its address",
    );
    let e = start_refused(rndis(query, 0, |b| put32(b, VALUE_OFFSET, 100)));
    assert_malformed(
        &e,
        "a value of 6 bytes at byte 100 in an answer of 30 bytes",
    );
    let e = start_refused(rndis(query, 0, |b| put32(b, VALUE_OFFSET, u32::MAX)));
    assert_malformed(&e, "a value of 6 bytes at byte 4294967295");
    let e = start_refused(rndis(query, 0, |b| put32(b, VALUE_LENGTH, u32::MAX)));
    assert_malformed(&e, "a value of 4294967295 bytes at byte 16");
    // The answer ends before its fields do.
    let e = start_refused(rndis(query, 0, |b| resize(b, 20)));
    assert_malformed(&e, "20 bytes in answer to the query of its address");
}

#[test]
fn an_mtu_that_is_none_is_refused() {
    let query = rndisprot::MESSAGE_TYPE_QUERY_CMPLT;
    let e = start_refused(rndis(query, 1, |b| {
        resize(b, 30);
        put32(b, VALUE_LENGTH, 6);
    }));
    assert_malformed(&e, "an MTU of 6 bytes");
    let e = start_refused(rndis(query, 1, |b| {
        resize(b, 26);
        put32(b, VALUE_LENGTH, 2);
    }));
    assert_malformed(&e, "an MTU of 2 bytes");
    let e = start_refused(rndis(query, 1, |b| put32(b, VALUE, 0)));
    assert_malformed(&e, "an MTU of 0, with receive sections of 1806 bytes");
    // A frame of the MTU, behind its RNDIS fields, must fit a receive section: 1,806
    // bytes hold an MTU of up to 1,748.
    for mtu in [1749, 9000, u32::MAX] {
        let e = start_refused(rndis(query, 1, move |b| put32(b, VALUE, mtu)));
        assert_malformed(
            &e,
            &format!("an MTU of {mtu}, with receive sections of 1806 bytes"),
        );
    }
    let (card, _host) = start_with(SMALL, rndis(query, 1, |b| put32(b, VALUE, 1748)));
    assert_eq!(up(card).mtu(), 1748);
}

#[test]
fn an_rndis_completion_before_its_request_is_refused() {
    let stray = || {
        rndis_message(
            rndisprot::MESSAGE_TYPE_SET_CMPLT,
            &rndisprot::SetComplete {
                request_id: 3,
                status: rndisprot::STATUS_SUCCESS,
            },
            &[],
        )
    };
    // One send section, which the host gives back only once the guest waits, and then
    // with a completion of a request not yet made: the guest reads it while it waits
    // for the section.
    let one = Settings {
        send_buffer: 2 * PAGE,
        ..SMALL
    };
    let (card, _host) = start_with(one, |host| {
        host.completes = Completes::OnWait;
        host.mangle = on(Step::Answer(RNDIS), 0, move |host, _| {
            host.then.push(stray());
            Fate::Send
        });
    });
    assert_malformed(&down(card), "an RNDIS completion of a request nobody made");
    // A host that stops reading with the ring to it full, and has sent a completion of
    // a request not yet made: the guest reads it while it waits for room.
    let (card, _host) = start_on(PAGE, 16 * PAGE, SMALL, |host| {
        host.wakes_on_wait = true;
        host.mangle = on(
            Step::Rndis(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT),
            0,
            move |host, _| {
                for serial in 0..170 {
                    host.offer_vf(serial);
                }
                host.paused = true;
                host.then.push(stray());
                Fate::Send
            },
        );
    });
    assert_malformed(&down(card), "an RNDIS completion of a request nobody made");
}
