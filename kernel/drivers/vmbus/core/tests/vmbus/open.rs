//! Opening a channel on a ring pair, closing it, and the signal in each direction.

use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::Error;
use moruna_vmbus::Event;
use moruna_vmbus::MemoryId;
use moruna_vmbus::MessageType;
use moruna_vmbus::RingPair;

use crate::host::FakeHost;
use crate::host::connected;
use crate::host::connection_of;
use crate::host::drain;

const CHANNEL: ChannelId = ChannelId(1);
const OTHER: ChannelId = ChannelId(2);

/// What the guest posted after the connection was made.
fn since_connected(host: &FakeHost) -> Vec<MessageType> {
    host.types()[2..].to_vec()
}

/// A connected client and host, and a ring pair of 8 pages, 4 each way.
fn with_ring() -> (Client, FakeHost, RingPair) {
    let (client, mut host) = connected();
    let memory = host.add_memory(1, 0x8000, 8);
    let ring = RingPair {
        memory,
        outgoing_pages: 4,
    };
    (client, host, ring)
}

#[test]
fn open_shares_the_rings_and_then_asks_for_the_channel() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert!(!client.is_open(CHANNEL));

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
    // The ring's GPADL first, and only then the open: the strict host has checked that
    // the open targets CPU 0 and splits the GPADL at the incoming ring's first page.
    assert_eq!(
        since_connected(&host),
        [MessageType::GPADL_HEADER, MessageType::OPEN_CHANNEL]
    );
    assert!(client.is_open(CHANNEL) && host.is_open(CHANNEL));
    assert!(!client.is_open(OTHER));
    let gpadl = host.ring_gpadl(CHANNEL).unwrap();
    assert_eq!(
        host.gpadl_pages(gpadl),
        Some((0x8000..0x8008).collect::<Vec<u64>>())
    );
}

#[test]
fn a_ring_pair_larger_than_one_message_opens_the_same_way() {
    let (mut client, mut host) = connected();
    // 1 MiB each way and the two control pages: 514 pages.
    let memory = host.add_memory(1, 0x10_0000, 514);
    let ring = RingPair {
        memory,
        outgoing_pages: 257,
    };
    client.open(&mut host, CHANNEL, ring).unwrap();

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
    let messages = since_connected(&host);
    assert_eq!(messages.len(), 1 + (514usize - 26).div_ceil(28) + 1);
    assert_eq!(messages.last(), Some(&MessageType::OPEN_CHANNEL));
}

#[test]
fn each_ring_needs_a_control_page_and_data() {
    let (mut client, mut host) = connected();
    let eight = host.add_memory(1, 0x1000, 8);
    let three = host.add_memory(2, 0x2000, 3);
    let four = host.add_memory(3, 0x3000, 4);
    let ring = |memory, outgoing_pages| RingPair {
        memory,
        outgoing_pages,
    };

    for (memory, pages, outgoing) in [
        (eight, 8, 0),
        (eight, 8, 1),
        (eight, 8, 7),
        (eight, 8, 8),
        (eight, 8, 9),
        (eight, 8, u32::MAX),
        (three, 3, 2),
    ] {
        assert_eq!(
            client.open(&mut host, CHANNEL, ring(memory, outgoing)),
            Err(Error::RingPages { pages, outgoing }),
        );
    }
    assert_eq!(
        client.open(&mut host, CHANNEL, ring(MemoryId(9), 2)),
        Err(Error::UnknownMemory(MemoryId(9)))
    );
    assert_eq!(
        client.open(&mut host, ChannelId(9), ring(eight, 4)),
        Err(Error::UnknownChannel(ChannelId(9)))
    );
    assert_eq!(
        since_connected(&host),
        [],
        "nothing was sent for any of them"
    );

    // The smallest pair there is: two pages each way.
    client.open(&mut host, CHANNEL, ring(four, 2)).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
}

#[test]
fn a_channel_is_opened_once() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    // While it opens, and once it is open.
    assert_eq!(
        client.open(&mut host, CHANNEL, ring),
        Err(Error::NotClosed(CHANNEL))
    );
    drain(&mut client, &mut host);
    assert_eq!(
        client.open(&mut host, CHANNEL, ring),
        Err(Error::NotClosed(CHANNEL))
    );
    assert_eq!(
        since_connected(&host),
        [MessageType::GPADL_HEADER, MessageType::OPEN_CHANNEL]
    );
}

#[test]
fn an_open_the_host_refuses_takes_the_rings_back() {
    let (mut client, mut host, ring) = with_ring();
    host.fail_open = Some(0xc000_009a);
    client.open(&mut host, CHANNEL, ring).unwrap();

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::OpenFailed {
            channel: CHANNEL,
            status: 0xc000_009a_u32 as i32
        }]
    );
    assert_eq!(
        since_connected(&host),
        [
            MessageType::GPADL_HEADER,
            MessageType::OPEN_CHANNEL,
            MessageType::GPADL_TEARDOWN
        ]
    );
    assert!(!client.is_open(CHANNEL) && !host.is_open(CHANNEL));
    assert_eq!(host.gpadl_count(), 0);

    // The channel is as it was offered: it can be opened again.
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
}

#[test]
fn rings_the_host_refuses_fail_the_open_before_it_is_asked() {
    let (mut client, mut host, ring) = with_ring();
    host.fail_gpadl = Some(-5);
    client.open(&mut host, CHANNEL, ring).unwrap();

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::OpenFailed {
            channel: CHANNEL,
            status: -5
        }]
    );
    assert_eq!(since_connected(&host), [MessageType::GPADL_HEADER]);
    assert!(!client.is_open(CHANNEL));

    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
}

#[test]
fn close_asks_the_host_and_then_takes_the_rings_back() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);

    client.close(CHANNEL).unwrap();
    assert!(!client.is_open(CHANNEL));
    // Closing: neither open nor closed until the rings are back.
    assert_eq!(client.close(CHANNEL), Err(Error::NotOpen(CHANNEL)));
    assert_eq!(
        client.open(&mut host, CHANNEL, ring),
        Err(Error::NotClosed(CHANNEL))
    );

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Closed { channel: CHANNEL }]
    );
    assert_eq!(
        since_connected(&host),
        [
            MessageType::GPADL_HEADER,
            MessageType::OPEN_CHANNEL,
            MessageType::CLOSE_CHANNEL,
            MessageType::GPADL_TEARDOWN
        ]
    );
    assert!(!host.is_open(CHANNEL));
    assert_eq!(host.gpadl_count(), 0);
    assert!(host.knows(CHANNEL), "the offer stands");

    // And it opens again.
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
}

#[test]
fn close_needs_an_open_channel() {
    let (mut client, mut host, ring) = with_ring();
    assert_eq!(client.close(CHANNEL), Err(Error::NotOpen(CHANNEL)));
    assert_eq!(
        client.close(ChannelId(9)),
        Err(Error::UnknownChannel(ChannelId(9)))
    );
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(client.close(CHANNEL), Err(Error::NotOpen(CHANNEL)));
    assert_eq!(since_connected(&host), []);
}

#[test]
fn the_rings_are_torn_down_by_close_alone() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);
    let rings = host.ring_gpadl(CHANNEL).unwrap();

    assert_eq!(
        client.teardown_gpadl(CHANNEL, rings),
        Err(Error::GpadlBusy(rings))
    );
    assert!(client.is_open(CHANNEL) && host.is_open(CHANNEL));
}

#[test]
fn a_gpadl_of_the_channel_outlives_its_close() {
    let (mut client, mut host, ring) = with_ring();
    let buffer = host.add_memory(2, 0x9000, 16);
    client.open(&mut host, CHANNEL, ring).unwrap();
    let gpadl = client.create_gpadl(&mut host, CHANNEL, buffer).unwrap();
    drain(&mut client, &mut host);

    client.close(CHANNEL).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Closed { channel: CHANNEL }]
    );
    assert_eq!(
        host.gpadl_count(),
        1,
        "the caller's GPADL is the caller's to end"
    );
    client.teardown_gpadl(CHANNEL, gpadl).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlTorndown {
            channel: CHANNEL,
            gpadl
        }]
    );
}

#[test]
fn the_host_is_signalled_on_the_connection_of_the_channels_offer() {
    let (mut client, mut host, ring) = with_ring();
    assert_eq!(
        client.signal_host(&mut host, CHANNEL),
        Err(Error::NotOpen(CHANNEL))
    );
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);

    client.signal_host(&mut host, CHANNEL).unwrap();
    client.signal_host(&mut host, CHANNEL).unwrap();
    assert_eq!(
        host.signals,
        [(connection_of(CHANNEL), 0), (connection_of(CHANNEL), 0)]
    );
    assert_eq!(
        client.signal_host(&mut host, OTHER),
        Err(Error::NotOpen(OTHER))
    );
    assert_eq!(
        client.signal_host(&mut host, ChannelId(9)),
        Err(Error::UnknownChannel(ChannelId(9)))
    );
    assert_eq!(host.signals.len(), 2);
}

#[test]
fn the_hosts_signal_is_the_flag_of_the_channels_id_taken_once() {
    let (mut client, mut host) = connected();
    for (id, channel) in [(1, CHANNEL), (2, OTHER)] {
        let memory = host.add_memory(id, 0x1000 * id, 4);
        let ring = RingPair {
            memory,
            outgoing_pages: 2,
        };
        client.open(&mut host, channel, ring).unwrap();
    }
    drain(&mut client, &mut host);
    assert_eq!(client.take_signals(&mut host), []);

    host.signal_guest(OTHER);
    assert_eq!(client.take_signals(&mut host), [OTHER]);
    assert_eq!(client.take_signals(&mut host), [], "taking it cleared it");

    host.signal_guest(CHANNEL);
    host.signal_guest(OTHER);
    assert_eq!(client.take_signals(&mut host), [CHANNEL, OTHER]);

    // A flag for a channel that is not open is not the client's to clear.
    client.close(OTHER).unwrap();
    drain(&mut client, &mut host);
    host.signal_guest(OTHER);
    assert_eq!(client.take_signals(&mut host), []);
    assert!(host.test_and_clear(OTHER));
}
