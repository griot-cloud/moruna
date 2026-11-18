//! Initialisation: NVSP in netvsc's order, the version offered newest first and never
//! below 5, then RNDIS, with the address and the MTU read from the host.

use guestmem_core::MemoryWrite;
use moruna_netvsc::{Buffer, Netvsc, NetvscError, Shared};
use moruna_vmbus::{Channel, ChannelError, MemoryId, Wait, testing::Memory};
use netvsp_protocol::{
    protocol::{self, Version},
    rndisprot,
};
use vmbus_ring::{OutgoingPacket, OutgoingPacketType};
use zerocopy::IntoBytes;

use crate::fake::{
    Bytes, Completes, DEADLINE, Fate, Got, MAC, PAGE, ROOMY, SMALL, Settings, Step, assert_clean,
    down, drain, on, parts, poll, start_on, start_with, started, up,
};

const RNDIS: u32 = protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET;
/// What the host says of a virtual function, and the message that would move the data
/// path to one.
const VF_ASSOCIATION: u32 = protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION;
const SWITCH_DATA_PATH: u32 = protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH;

#[test]
fn the_card_starts_in_netvscs_order_on_the_newest_version() {
    let (mut card, host) = started(SMALL);
    assert_clean(&host);
    assert!(card.signals() > 0);
    let host = host.borrow();
    // One offer, of the newest version the protocol crate defines.
    assert_eq!(host.offers, [Version::V61 as u32]);
    assert_eq!(host.version(), Some(Version::V61 as u32));
    // NVSP: the version, the NDIS configuration, the NDIS version, the receive buffer,
    // the send buffer. Then RNDIS: initialise, the address, the MTU, the filter.
    assert_eq!(
        host.messages,
        [
            protocol::MESSAGE_TYPE_INIT,
            protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
            protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
            protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
            protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
            RNDIS,
            RNDIS,
            RNDIS,
            RNDIS,
        ]
    );
    // Standard Ethernet's frame, nothing offered, NDIS 6.30.
    let (config, ndis) = host.ndis();
    assert_eq!(config.mtu, 1514);
    assert_eq!(u64::from(config.capabilities), 0);
    assert_eq!((ndis.ndis_major_version, ndis.ndis_minor_version), (6, 30));
    // What the card learned.
    assert_eq!(card.mac(), MAC);
    assert_eq!(card.mtu(), 1500);
    assert!(card.link_up());
    // Directed, multicast and broadcast.
    assert_eq!(
        host.filter,
        rndisprot::NDIS_PACKET_TYPE_DIRECTED
            | rndisprot::NDIS_PACKET_TYPE_MULTICAST
            | rndisprot::NDIS_PACKET_TYPE_BROADCAST
    );
    assert_eq!(host.filter, 0xb);
    // Every receive of an RNDIS completion was given back.
    assert_eq!(host.received, [protocol::Status::SUCCESS; 4]);
}

#[test]
fn the_versions_offered_are_all_of_the_protocols_from_5_up_newest_first() {
    // The step-down below walks exactly these. A version the protocol crate gains must
    // be added to the offers, and this fails until it is.
    assert!(Version::V61 > Version::V6 && Version::V6 > Version::V5 && Version::V5 > Version::V4);
    assert_eq!((Version::V61.major(), Version::V61.minor()), (6, 1));
    assert_eq!(Version::V5.major(), 5);
}

#[test]
fn the_card_steps_down_to_the_version_the_host_accepts() {
    // A host that knows nothing newer than NVSP 5 refuses 6.1 and 6.0.
    let (card, host) = start_with(SMALL, |host| {
        host.versions = vec![
            Version::V1 as u32,
            Version::V2 as u32,
            Version::V4 as u32,
            Version::V5 as u32,
        ];
    });
    let mut card = up(card);
    assert_clean(&host);
    assert_eq!(
        host.borrow().offers,
        [Version::V61 as u32, Version::V6 as u32, Version::V5 as u32]
    );
    assert_eq!(host.borrow().version(), Some(Version::V5 as u32));
    assert_eq!(card.mac(), MAC);
    // The card works at the older version's packet size, which the host checks.
    let frame = vec![0x5a; 100];
    assert!(card.send(&frame).unwrap());
    host.borrow_mut().inject(&frame);
    assert_eq!(drain(&mut card), [Got::Frame(frame.clone())]);
    assert_eq!(host.borrow().frames, [frame]);
    assert_clean(&host);

    // A host that accepts 6.0 is offered 6.1 first, and nothing after 6.0.
    let (card, host) = start_with(SMALL, |host| {
        host.versions = vec![Version::V5 as u32, Version::V6 as u32];
    });
    up(card);
    assert_clean(&host);
    assert_eq!(
        host.borrow().offers,
        [Version::V61 as u32, Version::V6 as u32]
    );
    assert_eq!(host.borrow().version(), Some(Version::V6 as u32));
}

#[test]
fn a_host_below_nvsp_5_is_refused() {
    let (card, host) = start_with(SMALL, |host| {
        host.versions = vec![Version::V1 as u32, Version::V2 as u32, Version::V4 as u32];
    });
    let e = down(card);
    assert!(matches!(e, NetvscError::Version), "{e}");
    assert_eq!(e.to_string(), "the host accepts no NVSP version from 5 up");
    // It was offered 6.1, 6.0 and 5.0, and nothing older, though it accepts 4.
    let host = host.borrow();
    assert_eq!(
        host.offers,
        [Version::V61 as u32, Version::V6 as u32, Version::V5 as u32]
    );
    assert_eq!(host.version(), None);
    assert!(host.violations.is_empty(), "{:#?}", host.violations);
}

#[test]
fn the_address_and_the_mtu_are_the_hosts() {
    let (card, host) = start_with(SMALL, |host| host.mtu = Some(1400));
    let mut card = up(card);
    assert_clean(&host);
    assert_eq!(card.mac(), [0x00, 0x15, 0x5d, 0x01, 0x02, 0x03]);
    assert_eq!(card.mtu(), 1400);
    // The largest frame is the MTU and an Ethernet header.
    assert!(card.send(&vec![0; 1414]).unwrap());
    let e = card.send(&vec![0; 1415]).unwrap_err();
    assert!(matches!(e, NetvscError::Length(1415)), "{e}");
    assert_eq!(host.borrow().frames, [vec![0; 1414]]);
}

#[test]
fn the_sections_are_the_ones_the_host_chose() {
    // Send sections of 2,048 bytes in four pages: eight of them. Receive sections of
    // 2,048 bytes in 64 pages: 128, of which 112 are for frames.
    let (card, host) = start_with(
        Settings {
            receive_buffer: 64 * PAGE,
            send_buffer: 4 * PAGE,
        },
        |host| {
            host.send_section = 2048;
            host.receive_section = Some(2048);
        },
    );
    let mut card = up(card);
    assert_eq!(host.borrow().free_sections(), 112);
    host.borrow_mut().hold = true;
    let frame = |n: u8| vec![n; 1514];
    for n in 0..8 {
        assert!(card.send(&frame(n)).unwrap(), "section {n}");
    }
    assert!(!card.send(&frame(8)).unwrap());
    host.borrow_mut().release();
    assert_eq!(drain(&mut card), []);
    assert!(card.send(&frame(8)).unwrap());
    assert_eq!(host.borrow().frames, (0..9).map(frame).collect::<Vec<_>>());
    // A frame comes in from a section of the host's size.
    host.borrow_mut().inject(&frame(9));
    assert_eq!(drain(&mut card), [Got::Frame(frame(9))]);
    assert_clean(&host);
}

#[test]
fn buffers_that_are_not_whole_pages_are_refused() {
    for (settings, what) in [
        (
            Settings {
                receive_buffer: 0,
                ..SMALL
            },
            "the receive buffer is not whole pages",
        ),
        (
            Settings {
                receive_buffer: 8 * PAGE + 1,
                ..SMALL
            },
            "the receive buffer is not whole pages",
        ),
        (
            Settings {
                send_buffer: 0,
                ..SMALL
            },
            "the send buffer is not whole pages",
        ),
        (
            Settings {
                send_buffer: PAGE / 2,
                ..SMALL
            },
            "the send buffer is not whole pages",
        ),
    ] {
        let (card, host) = start_with(settings, |_| ());
        let e = down(card);
        assert!(matches!(e, NetvscError::Buffer(why) if why == what), "{e}");
        assert_eq!(e.to_string(), what);
        // Nothing was sent.
        assert!(host.borrow().messages.is_empty());
    }
}

#[test]
fn a_channel_that_cannot_share_a_buffer_fails_the_start() {
    for shares in [0, 1] {
        let (card, host) = start_with(SMALL, |host| host.shares_left = shares);
        let e = down(card);
        assert!(
            matches!(e, NetvscError::Channel(ChannelError::NotShared)),
            "{e}"
        );
        assert_eq!(
            e.to_string(),
            "the memory could not be shared with the host"
        );
        // The first failure is before the receive buffer, the second before the send
        // buffer.
        let sent = host.borrow().messages.len();
        assert_eq!(sent, 3 + shares);
    }
    // A buffer the channel was never told of.
    let parts = parts(16 * PAGE, 16 * PAGE, SMALL);
    let stranger = Buffer {
        id: MemoryId(99),
        memory: parts.send.memory.clone(),
    };
    let card = Netvsc::start(parts.channel, parts.receive, stranger, Some(DEADLINE));
    assert!(matches!(
        card,
        Err(NetvscError::Channel(ChannelError::NotShared))
    ));
}

#[test]
fn a_host_that_never_answers_ends_the_start_at_the_deadline() {
    // The host drops its answer to each step in turn. The guest waits until the
    // deadline, which on this channel has passed once the host has nothing to say.
    let steps = [
        (
            Step::Answer(protocol::MESSAGE_TYPE_INIT),
            0,
            "the NVSP version",
        ),
        (
            Step::Answer(protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG),
            0,
            "the NDIS configuration",
        ),
        (
            Step::Answer(protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION),
            0,
            "the NDIS version",
        ),
        (
            Step::Answer(protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER),
            0,
            "the receive buffer",
        ),
        (
            Step::Answer(protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER),
            0,
            "the send buffer",
        ),
        (
            Step::Rndis(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT),
            0,
            "RNDIS initialisation",
        ),
        (
            Step::Rndis(rndisprot::MESSAGE_TYPE_QUERY_CMPLT),
            0,
            "the query of its address",
        ),
        (
            Step::Rndis(rndisprot::MESSAGE_TYPE_QUERY_CMPLT),
            1,
            "the query of its MTU",
        ),
        (
            Step::Rndis(rndisprot::MESSAGE_TYPE_SET_CMPLT),
            0,
            "the packet filter",
        ),
    ];
    for (step, nth, name) in steps {
        let (card, _host) = start_with(SMALL, |host| {
            host.mangle = on(step, nth, |_, _| Fate::Drop);
        });
        let e = down(card);
        assert!(
            matches!(e, NetvscError::Timeout(what) if what == name),
            "{step:?}: {e}"
        );
        assert_eq!(
            e.to_string(),
            format!("the host did not answer {name} in time")
        );
    }
}

#[test]
fn a_channel_the_host_takes_back_ends_the_start() {
    // Before the card has said a word: its first signal finds the channel closed.
    let (card, _host) = start_with(SMALL, |host| host.rescind = true);
    let e = down(card);
    assert!(
        matches!(e, NetvscError::Channel(ChannelError::Closed)),
        "{e}"
    );
    assert_eq!(e.to_string(), "the channel is closed");
    // While the card waits for an answer: its wait does.
    let version = protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION;
    let (card, _host) = start_with(SMALL, |host| {
        host.mangle = on(Step::Answer(version), 0, |host, _| {
            host.rescind = true;
            Fate::Drop
        });
    });
    let e = down(card);
    assert!(
        matches!(e, NetvscError::Channel(ChannelError::Closed)),
        "{e}"
    );
    // Before the card shares its send buffer: the share does.
    let buffer = protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER;
    let (card, _host) = start_with(SMALL, |host| {
        host.mangle = on(Step::Answer(buffer), 0, |host, _| {
            host.rescind = true;
            Fate::Send
        });
    });
    let e = down(card);
    assert!(
        matches!(e, NetvscError::Channel(ChannelError::Closed)),
        "{e}"
    );
}

#[test]
fn a_receive_buffer_the_host_cannot_use_is_refused_by_name() {
    // Four pages hold nine sections, and netvsp keeps sixteen for control alone.
    let (card, host) = start_with(
        Settings {
            receive_buffer: 4 * PAGE,
            ..SMALL
        },
        |_| (),
    );
    let e = down(card);
    assert!(
        matches!(
            e,
            NetvscError::Refused {
                step: "the receive buffer",
                status: 2,
                nvsp: Some(0x60001)
            }
        ),
        "{e}"
    );
    // The refusal says the versions in use, for the console.
    assert_eq!(
        e.to_string(),
        "the host refused the receive buffer with status 0x00000002 (NVSP 6.1, NDIS 6.30)"
    );
    assert_clean(&host);
}

#[test]
fn the_receive_buffer_has_an_id_azures_host_takes_and_every_receive_names_it() {
    // Azure's host refused a receive buffer whose ID was 0 with FAILURE (console
    // 2026-11-18T0146), and the fake refuses one so too: the card gives Linux's 0xcafe.
    let (mut card, host) = started(SMALL);
    assert_eq!(host.borrow().receive_id, Some(0xcafe));
    assert_eq!(card.nvsp_version(), 0x60001);
    assert_eq!(moruna_netvsc::Nvsp(card.nvsp_version()).to_string(), "6.1");
    // The host names the buffer by that ID in each receive, and the card takes them.
    host.borrow_mut().inject(&[9; 60]);
    assert_eq!(drain(&mut card), [Got::Frame(vec![9; 60])]);
    assert_clean(&host);
}

#[test]
fn the_start_says_each_step_and_what_the_host_answered() {
    let mut first = parts(16 * PAGE, 16 * PAGE, SMALL);
    let host = first.host.clone();
    host.borrow_mut().flush(first.channel.host());
    let mut lines = Vec::new();
    let card = Netvsc::start_saying(
        first.channel,
        first.receive,
        first.send,
        Some(DEADLINE),
        &mut |line| lines.push(line.to_string()),
    );
    assert!(card.is_ok());
    assert_eq!(
        lines,
        [
            "NVSP 6.1 offered",
            "NVSP 6.1 accepted",
            "NDIS configuration sent: frames of 1514 bytes",
            "NDIS configuration completed; NDIS version 6.30 sent",
            "NDIS version completed",
            "receive buffer: sharing 32768 bytes",
            "receive buffer: GPADL 0x1 made; sent with ID 0xcafe",
            "receive buffer accepted: 18 sections of 1806 bytes from byte 0",
            "send buffer: sharing 16384 bytes",
            "send buffer: GPADL 0x2 made; sent with ID 0x0",
            "send buffer accepted: 2 sections of 6144 bytes",
            "RNDIS initialisation sent",
            "RNDIS initialised",
            "address queried",
            "address 00:15:5d:01:02:03",
            "MTU queried",
            "MTU 1500",
            "packet filter 0xb sent",
            "packet filter set",
            "link up",
        ]
    );
    // A start that stops is seen to stop: the last line says where.
    let older = parts(16 * PAGE, 16 * PAGE, SMALL);
    older.host.borrow_mut().versions = vec![Version::V6 as u32];
    let mut lines = Vec::new();
    let _ = Netvsc::start_saying(
        older.channel,
        older.receive,
        older.send,
        Some(DEADLINE),
        &mut |line| lines.push(line.to_string()),
    );
    assert_eq!(
        lines[..3],
        [
            "NVSP 6.1 offered",
            "NVSP 6.1 refused with status 0x0",
            "NVSP 6.0 offered"
        ]
    );
}

#[test]
fn no_packet_asks_for_a_completion_under_transaction_0() {
    // Hyper-V neither answers nor acts on such a packet, and the fake does as it does:
    // netvsc's first RNDIS request went in section 0 under ID 0, and Azure's host never
    // answered it (console 2026-11-18T0213). Each section's transaction is its index
    // plus one. The two sections carry frames over and over, each completed.
    let (mut card, host) = started(SMALL);
    for round in 0..4u8 {
        assert!(card.send(&[round; 60]).unwrap());
        assert!(card.send(&[round + 10; 60]).unwrap());
        assert_eq!(drain(&mut card), []);
    }
    assert_eq!(host.borrow().ignored, 0);
    assert_eq!(host.borrow().frames.len(), 8);
    assert!(
        host.borrow().strict,
        "a host that reads only when signalled"
    );
    assert_clean(&host);
}

#[test]
fn a_virtual_function_offered_during_initialisation_is_acknowledged_and_ignored() {
    // netvsp associates the virtual function right after RNDIS is initialised.
    let (card, host) = start_with(SMALL, |host| host.vf = Some(7));
    let mut card = up(card);
    assert_clean(&host);
    assert_eq!(host.borrow().vf_acknowledged, 1);
    assert!(!host.borrow().messages.contains(&SWITCH_DATA_PATH));
    assert!(!host.borrow().messages.contains(&VF_ASSOCIATION));
    // The card carries frames on the synthetic path.
    let frame = vec![7; 60];
    assert!(card.send(&frame).unwrap());
    host.borrow_mut().inject(&frame);
    assert_eq!(drain(&mut card), [Got::Frame(frame.clone())]);
    assert_eq!(host.borrow().frames, [frame]);
    assert!(!host.borrow().messages.contains(&SWITCH_DATA_PATH));
    assert_clean(&host);
}

#[test]
fn the_link_is_as_the_host_last_said_when_the_card_starts() {
    // The host indicates the link down before it answers the address query.
    let (card, host) = start_with(SMALL, |host| {
        host.mangle = on(
            Step::Rndis(rndisprot::MESSAGE_TYPE_QUERY_CMPLT),
            0,
            |host, _| {
                host.link(false);
                Fate::Send
            },
        );
    });
    let mut card = up(card);
    assert_clean(&host);
    assert!(!card.link_up());
    // Nothing is reported for it now: the card started with the link down.
    assert_eq!(poll(&mut card).unwrap(), None);
    host.borrow_mut().link(true);
    assert_eq!(drain(&mut card), [Got::Link(true)]);
    assert!(card.link_up());
}

#[test]
fn frames_that_arrive_before_the_card_has_started_are_dropped_and_their_sections_returned() {
    // One frame before the MTU is known, and one as the filter is set.
    let early = vec![1; 1514];
    let late = vec![2; 60];
    let (first, second) = (early.clone(), late.clone());
    let (card, host) = start_with(SMALL, move |host| {
        let mut frames = vec![second, first];
        host.mangle = Some(Box::new(move |host, step, _| {
            let now = matches!(
                step,
                Step::Rndis(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT)
                    | Step::Rndis(rndisprot::MESSAGE_TYPE_SET_CMPLT)
            );
            if now {
                let frame = frames.pop().unwrap();
                host.deliver(&[&frame]);
            }
            Fate::Send
        }));
    });
    let mut card = up(card);
    assert_clean(&host);
    // Neither frame is handed over, and both sections are the host's again.
    assert_eq!(poll(&mut card).unwrap(), None);
    assert_eq!(host.borrow().free_sections(), 2);
    assert_eq!(host.borrow().received, [protocol::Status::SUCCESS; 6]);
    // The next frames are.
    host.borrow_mut().inject(&early);
    host.borrow_mut().inject(&late);
    assert_eq!(drain(&mut card), [Got::Frame(early), Got::Frame(late)]);
}

#[test]
fn one_send_section_is_enough_whenever_the_host_completes_a_request() {
    // Two pages hold one send section of 6,144 bytes, so each RNDIS request must wait
    // for the section the last one used.
    let one = Settings {
        send_buffer: 2 * PAGE,
        ..SMALL
    };
    for completes in [Completes::First, Completes::AfterAnswer, Completes::OnWait] {
        let (card, host) = start_with(one, |host| host.completes = completes);
        let mut card = up(card);
        assert_clean(&host);
        assert_eq!(card.mac(), MAC, "{completes:?}");
        assert_eq!(card.mtu(), 1500);
        // The host may still hold the section the last request used. The caller's
        // loop is a poll until nothing, then a wait, which brings it back.
        loop {
            assert_eq!(drain(&mut card), []);
            if card.wait() != Ok(Wait::Signalled) {
                break;
            }
        }
        // And the one section carries frames, one at a time.
        host.borrow_mut().hold = true;
        assert!(card.send(&[3; 64]).unwrap());
        assert!(!card.send(&[4; 64]).unwrap());
        host.borrow_mut().release();
        assert_eq!(drain(&mut card), []);
        assert!(card.send(&[4; 64]).unwrap());
        assert_eq!(host.borrow().frames, [vec![3; 64], vec![4; 64]]);
    }
}

#[test]
fn a_host_slow_to_read_does_not_stall_the_start() {
    // The ring to the host is one page. After RNDIS is initialised the host says 170
    // things of a virtual function and stops reading: the guest's acknowledgements fill
    // the ring, and its next request finds no room. The host reads again only once the
    // guest waits for it, and signals when the ring has room.
    let (card, host) = start_on(PAGE, 16 * PAGE, SMALL, |host| {
        host.wakes_on_wait = true;
        host.mangle = on(
            Step::Rndis(rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT),
            0,
            |host, _| {
                for serial in 0..170 {
                    host.offer_vf(serial);
                }
                host.paused = true;
                Fate::Send
            },
        );
    });
    let card = up(card);
    assert_clean(&host);
    assert_eq!(host.borrow().vf_acknowledged, 170);
    assert_eq!(card.mac(), MAC);
    assert_eq!(card.mtu(), 1500);
    assert!(!host.borrow().messages.contains(&SWITCH_DATA_PATH));
}

#[test]
fn the_caller_waits_on_the_cards_channel_between_polls() {
    let (mut card, host) = started(ROOMY);
    // The start waited for each of the host's answers, and took their signals. What came
    // with its last answer may be on the ring still: the caller polls before it waits.
    assert_eq!(drain(&mut card), []);
    // Nothing has been sent since.
    assert_eq!(card.wait(), Ok(Wait::TimedOut));
    host.borrow_mut().inject(&[9; 60]);
    assert_eq!(card.wait(), Ok(Wait::Signalled));
    assert_eq!(drain(&mut card), [Got::Frame(vec![9; 60])]);
}

#[test]
fn a_ring_to_the_host_that_is_full_when_the_card_starts_is_waited_for() {
    // The ring to the host is one page, full of packets the host has not read, and the
    // host reads again only once the guest waits for it. The card's first message has
    // no room, waits, and goes once the host has made some.
    let parts = parts(PAGE, 16 * PAGE, SMALL);
    let host = parts.host.clone();
    {
        let mut host = host.borrow_mut();
        host.lenient = true;
        host.paused = true;
        host.wakes_on_wait = true;
    }
    let leftover = [999u32.as_bytes(), &[0; 28]].concat();
    let outgoing = parts.channel.outgoing();
    let mut at = outgoing.outgoing().unwrap();
    let mut left = 0;
    let packet = OutgoingPacket {
        transaction_id: 1,
        size: leftover.len(),
        typ: OutgoingPacketType::InBandWithCompletion,
    };
    while let Ok(range) = outgoing.write(&mut at, &packet) {
        range.writer(outgoing).write(&leftover).unwrap();
        outgoing.commit_write(&mut at);
        left += 1;
    }
    // Each takes 56 bytes of the ring's 4,088.
    assert_eq!(left, 73);
    let mut card = up(parts.start());
    assert_eq!(card.mac(), MAC);
    assert_eq!(host.borrow().messages.len(), left + 9);
    assert!(card.send(&[1; 60]).unwrap());
    assert_eq!(host.borrow().frames, [vec![1; 60]]);
}

#[test]
fn a_start_with_no_deadline_needs_no_wait_from_a_host_that_answers_at_once() {
    // The test channel has no clock, and a wait with no deadline that finds no signal
    // would never end. This host answers each message as it is signalled, so the card
    // reads every answer without waiting at all.
    let parts = parts(16 * PAGE, 16 * PAGE, SMALL);
    let card = Netvsc::start(parts.channel, parts.receive, parts.send, None);
    let Ok(card) = card else {
        panic!("the card did not start");
    };
    assert_eq!(card.mac(), MAC);
    assert_clean(&parts.host);
}

#[test]
fn a_buffer_knows_its_length_and_reads_back_what_was_written() {
    let memory = Bytes {
        memory: Memory::new(3),
        len: 3 * PAGE,
    };
    assert_eq!(memory.len(), 3 * PAGE);
    assert!(!memory.is_empty());
    let mut bytes = [0xff; 4];
    memory.read(PAGE, &mut bytes);
    assert_eq!(bytes, [0; 4]);
    memory.write(PAGE, &[1, 2, 3, 4]);
    memory.read(PAGE, &mut bytes);
    assert_eq!(bytes, [1, 2, 3, 4]);
    let none = Bytes {
        memory: Memory::new(0),
        len: 0,
    };
    assert!(none.is_empty());
}
