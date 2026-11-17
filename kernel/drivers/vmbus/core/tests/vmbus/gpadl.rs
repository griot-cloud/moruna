//! GPADLs: creating one of any size the protocol allows, a refusal, and teardown.

use moruna_vmbus::ChannelId;
use moruna_vmbus::Error;
use moruna_vmbus::Event;
use moruna_vmbus::GpadlId;
use moruna_vmbus::MAX_PAGES;
use moruna_vmbus::MemoryId;
use moruna_vmbus::MessageType;
use vmbus_core::protocol;

use crate::host::FakeHost;
use crate::host::connected;
use crate::host::drain;

const CHANNEL: ChannelId = ChannelId(1);

/// What the guest posted after the connection was made.
fn since_connected(host: &FakeHost) -> Vec<MessageType> {
    host.types()[2..].to_vec()
}

/// Creates a GPADL of `pages` pages and gives the number of messages it took.
fn create(pages: usize) -> usize {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(7, 0x40_0000, pages);
    let gpadl = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlCreated {
            channel: CHANNEL,
            gpadl
        }]
    );
    // The host has exactly the memory's pages, in order.
    let expected: Vec<u64> = (0x40_0000..0x40_0000 + pages as u64).collect();
    assert_eq!(host.gpadl_pages(gpadl), Some(expected));

    let messages = since_connected(&host);
    assert_eq!(messages[0], MessageType::GPADL_HEADER);
    assert!(
        messages[1..]
            .iter()
            .all(|typ| *typ == MessageType::GPADL_BODY)
    );
    messages.len()
}

#[test]
fn a_gpadl_of_one_page_is_one_message() {
    assert_eq!(create(1), 1);
}

#[test]
fn the_most_pages_one_message_carries_is_twenty_six() {
    // A header carries 27 values: the range, and 26 page numbers.
    assert_eq!(create(26), 1);
}

#[test]
fn one_page_more_needs_a_second_message() {
    assert_eq!(create(27), 2);
}

#[test]
fn a_body_carries_twenty_eight_page_numbers() {
    assert_eq!(create(26 + 28), 2);
    assert_eq!(create(26 + 28 + 1), 3);
}

#[test]
fn a_gpadl_of_sixteen_mebibytes_is_many_messages() {
    // A network card's receive buffer: 4,096 pages, a header and 146 bodies.
    assert_eq!(create(4096), 1 + (4096 - 26usize).div_ceil(28));
}

#[test]
fn the_largest_gpadl_the_protocol_allows() {
    // 8,190 pages: with the range, 8,191 values, whose bytes are the most 16 bits count.
    assert_eq!(MAX_PAGES, 8190);
    assert_eq!(create(MAX_PAGES), 1 + (MAX_PAGES - 26).div_ceil(28));
}

#[test]
fn memory_no_gpadl_can_name_is_refused_before_anything_is_sent() {
    let (mut client, mut host) = connected();
    let too_large = host.add_memory(1, 0x1000, MAX_PAGES + 1);
    let empty = host.add_memory(2, 0x1000, 0);

    assert_eq!(
        client.create_gpadl(&mut host, CHANNEL, too_large),
        Err(Error::PageCount(MAX_PAGES + 1))
    );
    assert_eq!(
        client.create_gpadl(&mut host, CHANNEL, empty),
        Err(Error::PageCount(0))
    );
    assert_eq!(
        client.create_gpadl(&mut host, CHANNEL, MemoryId(3)),
        Err(Error::UnknownMemory(MemoryId(3)))
    );
    assert_eq!(
        client.create_gpadl(&mut host, ChannelId(9), too_large),
        Err(Error::UnknownChannel(ChannelId(9)))
    );
    assert_eq!(drain(&mut client, &mut host), []);
    assert_eq!(since_connected(&host), []);
    assert_eq!(host.gpadl_count(), 0);
}

#[test]
fn a_gpadl_the_host_refuses_is_reported_and_forgotten() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 40);
    host.fail_gpadl = Some(protocol::STATUS_UNSUCCESSFUL);
    let gpadl = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlFailed {
            channel: CHANNEL,
            gpadl,
            status: protocol::STATUS_UNSUCCESSFUL
        }]
    );
    assert_eq!(
        client.teardown_gpadl(CHANNEL, gpadl),
        Err(Error::UnknownGpadl(gpadl))
    );

    // The same memory can be offered again.
    let again = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();
    assert_ne!(again, gpadl);
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlCreated {
            channel: CHANNEL,
            gpadl: again
        }]
    );
}

#[test]
fn teardown_gives_the_memory_back() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 3);
    let gpadl = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();
    drain(&mut client, &mut host);

    client.teardown_gpadl(CHANNEL, gpadl).unwrap();
    // Asked once: a second teardown waits for nothing more.
    assert_eq!(
        client.teardown_gpadl(CHANNEL, gpadl),
        Err(Error::GpadlBusy(gpadl))
    );
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlTorndown {
            channel: CHANNEL,
            gpadl
        }]
    );
    assert_eq!(
        since_connected(&host),
        [MessageType::GPADL_HEADER, MessageType::GPADL_TEARDOWN]
    );
    assert_eq!(host.gpadl_count(), 0);
    assert_eq!(
        client.teardown_gpadl(CHANNEL, gpadl),
        Err(Error::UnknownGpadl(gpadl))
    );
}

#[test]
fn a_gpadl_is_not_torn_down_before_the_host_has_it() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 3);
    let gpadl = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();

    assert_eq!(
        client.teardown_gpadl(CHANNEL, gpadl),
        Err(Error::GpadlBusy(gpadl))
    );
    assert_eq!(
        client.teardown_gpadl(CHANNEL, GpadlId(77)),
        Err(Error::UnknownGpadl(GpadlId(77)))
    );
    // A GPADL belongs to its channel.
    drain(&mut client, &mut host);
    assert_eq!(
        client.teardown_gpadl(ChannelId(2), gpadl),
        Err(Error::UnknownGpadl(gpadl))
    );
}

#[test]
fn every_gpadl_of_the_connection_has_its_own_id() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 2);
    let a = client
        .create_gpadl(&mut host, ChannelId(1), memory)
        .unwrap();
    let b = client
        .create_gpadl(&mut host, ChannelId(2), memory)
        .unwrap();
    let c = client
        .create_gpadl(&mut host, ChannelId(1), memory)
        .unwrap();
    assert!(a != b && b != c && a != c);
    assert!(a.0 != 0 && b.0 != 0 && c.0 != 0);

    let events = drain(&mut client, &mut host);
    assert_eq!(
        events,
        [
            Event::GpadlCreated {
                channel: ChannelId(1),
                gpadl: a
            },
            Event::GpadlCreated {
                channel: ChannelId(2),
                gpadl: b
            },
            Event::GpadlCreated {
                channel: ChannelId(1),
                gpadl: c
            },
        ]
    );
    assert_eq!(host.gpadl_count(), 3);
}
