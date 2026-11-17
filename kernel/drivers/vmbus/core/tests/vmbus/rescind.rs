//! A rescinded offer: the channel is closed for the caller, what the host still holds
//! of it is taken back, and its ID is released.

use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::Error;
use moruna_vmbus::Event;
use moruna_vmbus::HostError;
use moruna_vmbus::MessageType;
use moruna_vmbus::RingPair;
use vmbus_core::OutgoingMessage;
use vmbus_core::protocol;

use crate::host::FakeHost;
use crate::host::NIC;
use crate::host::connected;
use crate::host::drain;
use crate::host::instance;
use crate::host::offer;

const CHANNEL: ChannelId = ChannelId(1);

/// What the guest posted after the connection was made.
fn since_connected(host: &FakeHost) -> Vec<MessageType> {
    host.types()[2..].to_vec()
}

fn with_ring() -> (Client, FakeHost, RingPair) {
    let (client, mut host) = connected();
    let memory = host.add_memory(1, 0x8000, 8);
    let ring = RingPair {
        memory,
        outgoing_pages: 4,
    };
    (client, host, ring)
}

/// Every request on a rescinded channel is refused, and nothing is sent for it.
fn refuses_everything(client: &mut Client, host: &mut FakeHost, ring: RingPair, error: Error) {
    let posted = host.posted.len();
    assert_eq!(client.open(host, CHANNEL, ring), Err(error));
    assert_eq!(client.close(CHANNEL), Err(error));
    assert_eq!(client.create_gpadl(host, CHANNEL, ring.memory), Err(error));
    assert_eq!(
        client.teardown_gpadl(CHANNEL, moruna_vmbus::GpadlId(1)),
        Err(error)
    );
    assert_eq!(client.signal_host(host, CHANNEL), Err(error));
    assert!(!client.is_open(CHANNEL));
    assert_eq!(host.posted.len(), posted);
    assert!(host.signals.is_empty());
}

#[test]
fn a_rescinded_offer_that_was_never_opened_is_released_at_once() {
    let (mut client, mut host, ring) = with_ring();
    host.rescind(CHANNEL);

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Rescinded { channel: CHANNEL }]
    );
    assert_eq!(since_connected(&host), [MessageType::REL_ID_RELEASED]);
    assert_eq!(host.released, [1]);
    assert_eq!(client.offers().count(), 1, "the other offer stands");
    assert!(client.offer(CHANNEL).is_none());
    refuses_everything(&mut client, &mut host, ring, Error::UnknownChannel(CHANNEL));
}

#[test]
fn a_rescind_while_open_closes_the_channel_and_tells_the_caller() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);
    assert!(client.is_open(CHANNEL));

    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [
            Event::Rescinded { channel: CHANNEL },
            Event::Closed { channel: CHANNEL }
        ]
    );
    // The rings come back with no close request, since the host closed the channel
    // itself; then the ID is released.
    assert_eq!(
        since_connected(&host),
        [
            MessageType::GPADL_HEADER,
            MessageType::OPEN_CHANNEL,
            MessageType::GPADL_TEARDOWN,
            MessageType::REL_ID_RELEASED
        ]
    );
    assert_eq!(host.released, [1]);
    assert_eq!(host.gpadl_count(), 0);
    assert!(!client.is_open(CHANNEL));
    assert_eq!(client.take_signals(&mut host), []);
    refuses_everything(&mut client, &mut host, ring, Error::UnknownChannel(CHANNEL));
}

#[test]
fn the_channel_is_refused_from_the_rescind_and_forgotten_when_its_rings_are_back() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);

    host.hold_teardown = true;
    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Rescinded { channel: CHANNEL }]
    );
    // Told, and not yet released: the host still holds the rings.
    assert_eq!(client.offers().count(), 1);
    assert!(client.offer(CHANNEL).is_some());
    assert!(host.released.is_empty());
    refuses_everything(&mut client, &mut host, ring, Error::Rescinded(CHANNEL));

    // A second rescind of the same offer is not the protocol's.
    host.send(OutgoingMessage::new(&protocol::RescindChannelOffer {
        channel_id: CHANNEL,
    }));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::AlreadyRescinded(CHANNEL))
    );

    host.release();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Closed { channel: CHANNEL }]
    );
    assert_eq!(host.released, [1]);
    assert!(client.offer(CHANNEL).is_none());
}

#[test]
fn a_rescind_takes_back_every_gpadl_of_the_channel() {
    let (mut client, mut host, ring) = with_ring();
    let buffer = host.add_memory(2, 0x9000, 40);
    client.open(&mut host, CHANNEL, ring).unwrap();
    let gpadl = client.create_gpadl(&mut host, CHANNEL, buffer).unwrap();
    drain(&mut client, &mut host);
    assert_eq!(host.gpadl_count(), 2);

    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [
            Event::Rescinded { channel: CHANNEL },
            Event::Closed { channel: CHANNEL },
            Event::GpadlTorndown {
                channel: CHANNEL,
                gpadl
            },
        ]
    );
    // Released only once both are gone: the strict host checks it holds none.
    assert_eq!(host.gpadl_count(), 0);
    assert_eq!(host.released, [1]);
    assert_eq!(
        since_connected(&host).last(),
        Some(&MessageType::REL_ID_RELEASED)
    );
}

#[test]
fn a_rescind_while_the_rings_are_on_their_way_takes_them_back_when_they_arrive() {
    let (mut client, mut host, ring) = with_ring();
    host.hold_gpadl = true;
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(drain(&mut client, &mut host), []);

    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Rescinded { channel: CHANNEL }]
    );
    assert!(
        host.released.is_empty(),
        "the host still has the rings to answer for"
    );
    refuses_everything(&mut client, &mut host, ring, Error::Rescinded(CHANNEL));

    host.release();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Closed { channel: CHANNEL }]
    );
    // No open was ever asked.
    assert_eq!(
        since_connected(&host),
        [
            MessageType::GPADL_HEADER,
            MessageType::GPADL_TEARDOWN,
            MessageType::REL_ID_RELEASED
        ]
    );
    assert_eq!(host.gpadl_count(), 0);
}

#[test]
fn a_rescind_while_the_open_is_on_its_way_takes_the_rings_back() {
    let (mut client, mut host, ring) = with_ring();
    host.hold_open = true;
    client.open(&mut host, CHANNEL, ring).unwrap();
    assert_eq!(drain(&mut client, &mut host), []);
    assert_eq!(
        since_connected(&host),
        [MessageType::GPADL_HEADER, MessageType::OPEN_CHANNEL]
    );

    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [
            Event::Rescinded { channel: CHANNEL },
            Event::Closed { channel: CHANNEL }
        ]
    );
    assert_eq!(host.released, [1]);
    assert!(!client.is_open(CHANNEL));

    // The open's answer, if the host still sends it, is about a channel that is gone.
    host.release();
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::UnknownChannel(CHANNEL))
    );
}

#[test]
fn a_rescind_while_closing_changes_nothing_but_the_release() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);

    host.hold_teardown = true;
    client.close(CHANNEL).unwrap();
    assert_eq!(drain(&mut client, &mut host), []);
    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Rescinded { channel: CHANNEL }]
    );

    host.release();
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
            MessageType::GPADL_TEARDOWN,
            MessageType::REL_ID_RELEASED
        ]
    );
}

#[test]
fn a_gpadl_that_arrives_after_the_rescind_is_taken_back() {
    let (mut client, mut host, _) = with_ring();
    let buffer = host.add_memory(2, 0x9000, 4);
    host.hold_gpadl = true;
    let gpadl = client.create_gpadl(&mut host, CHANNEL, buffer).unwrap();
    drain(&mut client, &mut host);

    host.rescind(CHANNEL);
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Rescinded { channel: CHANNEL }]
    );
    host.release();
    assert_eq!(
        drain(&mut client, &mut host),
        [
            Event::GpadlCreated {
                channel: CHANNEL,
                gpadl
            },
            Event::GpadlTorndown {
                channel: CHANNEL,
                gpadl
            }
        ]
    );
    assert_eq!(host.released, [1]);
}

#[test]
fn a_gpadl_refused_after_the_rescind_leaves_nothing_to_take_back() {
    // Once for a GPADL of the caller's, once for the rings of an open.
    for rings in [false, true] {
        let (mut client, mut host, ring) = with_ring();
        host.hold_gpadl = true;
        host.fail_gpadl = Some(protocol::STATUS_UNSUCCESSFUL);
        let gpadl = if rings {
            client.open(&mut host, CHANNEL, ring).unwrap();
            None
        } else {
            Some(
                client
                    .create_gpadl(&mut host, CHANNEL, ring.memory)
                    .unwrap(),
            )
        };
        drain(&mut client, &mut host);
        host.rescind(CHANNEL);
        drain(&mut client, &mut host);

        host.release();
        let expected = match gpadl {
            // The caller was told the channel is gone; its rings were never shared.
            None => Event::Closed { channel: CHANNEL },
            Some(gpadl) => Event::GpadlFailed {
                channel: CHANNEL,
                gpadl,
                status: protocol::STATUS_UNSUCCESSFUL,
            },
        };
        assert_eq!(drain(&mut client, &mut host), [expected]);
        assert_eq!(
            since_connected(&host),
            [MessageType::GPADL_HEADER, MessageType::REL_ID_RELEASED]
        );
        assert_eq!(host.released, [1]);
    }
}

#[test]
fn a_channel_id_is_offered_again_only_after_its_release() {
    let (mut client, mut host, ring) = with_ring();
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);
    host.hold_teardown = true;
    host.rescind(CHANNEL);
    drain(&mut client, &mut host);

    // The client has not released the ID yet: an offer that reuses it is not the
    // protocol's.
    host.send(offer(1));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::DuplicateOffer(CHANNEL))
    );

    host.release();
    drain(&mut client, &mut host);
    assert_eq!(host.released, [1]);
    // Released, the ID is the host's to use again, for the same device or another.
    host.send(offer(1));
    let events = drain(&mut client, &mut host);
    assert!(matches!(events[..], [Event::Offer(offer)] if offer.channel == CHANNEL));
    assert_eq!(client.offers().count(), 2);
    assert!(!client.is_open(CHANNEL));
}

#[test]
fn a_device_offered_after_a_release_is_a_new_channel() {
    let (mut client, mut host, ring) = with_ring();
    host.rescind(CHANNEL);
    drain(&mut client, &mut host);

    let again = host.offer(NIC, instance(1));
    assert_ne!(again, CHANNEL);
    let events = drain(&mut client, &mut host);
    assert!(matches!(events[..], [Event::Offer(offer)] if offer.channel == again));
    client.open(&mut host, again, ring).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: again }]
    );
}
