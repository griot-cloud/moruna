//! Garbage from the host: each piece is refused by name, without a panic, and leaves
//! the client able to go on.

use hvdef::HvMessageType;
use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::ClientState;
use moruna_vmbus::Error;
use moruna_vmbus::Event;
use moruna_vmbus::GpadlId;
use moruna_vmbus::HostError;
use moruna_vmbus::HvMessage;
use moruna_vmbus::MessageType;
use moruna_vmbus::RingPair;
use moruna_vmbus::Version;
use vmbus_core::OutgoingMessage;
use vmbus_core::protocol;

use crate::host::FakeHost;
use crate::host::NIC;
use crate::host::connected;
use crate::host::drain;
use crate::host::instance;
use crate::host::offer;

const CHANNEL: ChannelId = ChannelId(1);

/// The bytes of a VMBus message of type `typ` whose fields are `fields`.
fn raw(typ: u32, fields: &[u8]) -> Vec<u8> {
    let mut bytes = typ.to_le_bytes().to_vec();
    bytes.extend([0; 4]);
    bytes.extend(fields);
    bytes
}

/// Sends `bytes` and expects the client to refuse them with `error`.
fn refused(client: &mut Client, host: &mut FakeHost, bytes: &[u8], error: HostError) {
    host.send_bytes(bytes);
    assert_eq!(client.poll(host), Err(error));
    assert_eq!(
        client.poll(host),
        Ok(None),
        "the message is gone, and only it"
    );
}

/// The client still works: it opens the first channel and closes it again.
fn still_works(client: &mut Client, host: &mut FakeHost) {
    let memory = host.add_memory(0xfeed, 0x7_0000, 4);
    let ring = RingPair {
        memory,
        outgoing_pages: 2,
    };
    client.open(host, CHANNEL, ring).unwrap();
    assert_eq!(drain(client, host), [Event::Opened { channel: CHANNEL }]);
    client.close(CHANNEL).unwrap();
    assert_eq!(drain(client, host), [Event::Closed { channel: CHANNEL }]);
}

#[test]
fn a_message_that_is_not_vmbus_is_refused() {
    let (mut client, mut host) = connected();
    // A timer's expiry, and a slot with no type at all.
    for typ in [0x8000_0010, 0, 2, u32::MAX] {
        host.send_slot(HvMessage::new(
            HvMessageType(typ),
            0,
            &[1, 0, 0, 0, 0, 0, 0, 0],
        ));
        assert_eq!(client.poll(&mut host), Err(HostError::NotVmbus(typ)));
    }
    still_works(&mut client, &mut host);
}

#[test]
fn a_length_no_slot_can_hold_is_refused() {
    let (mut client, mut host) = connected();
    for len in [241, 255] {
        let mut slot = HvMessage::new(HvMessageType(1), 0, &[4, 0, 0, 0, 0, 0, 0, 0]);
        slot.header.len = len;
        host.send_slot(slot);
        assert_eq!(client.poll(&mut host), Err(HostError::Length(len)));
    }
    still_works(&mut client, &mut host);
}

#[test]
fn a_message_too_short_for_its_type_is_refused() {
    let (mut client, mut host) = connected();
    refused(&mut client, &mut host, &[], HostError::TooSmall(None));
    refused(
        &mut client,
        &mut host,
        &[1, 0, 0, 0],
        HostError::TooSmall(None),
    );
    refused(
        &mut client,
        &mut host,
        &[1, 0, 0, 0, 0, 0, 0],
        HostError::TooSmall(None),
    );
    // Every message with fields, cut one byte short of them.
    for (typ, size) in [
        (MessageType::OFFER_CHANNEL, 188),
        (MessageType::RESCIND_CHANNEL_OFFER, 4),
        (MessageType::OPEN_CHANNEL_RESULT, 12),
        (MessageType::GPADL_CREATED, 12),
        (MessageType::GPADL_TORNDOWN, 4),
        (MessageType::VERSION_RESPONSE, 8),
    ] {
        let short = raw(typ.0, &vec![0xff; size - 1]);
        refused(
            &mut client,
            &mut host,
            &short,
            HostError::TooSmall(Some(typ)),
        );
    }
    still_works(&mut client, &mut host);
}

#[test]
fn a_message_type_the_protocol_does_not_have_is_refused() {
    let (mut client, mut host) = connected();
    for typ in [0, 30, 99, 0x100, u32::MAX] {
        refused(
            &mut client,
            &mut host,
            &raw(typ, &[0; 64]),
            HostError::Unsupported(MessageType(typ)),
        );
    }
    still_works(&mut client, &mut host);
}

#[test]
fn before_contact_only_an_answer_to_it_is_a_message() {
    let mut host = FakeHost::new();
    let mut client = Client::new(0);
    // Disconnected.
    host.send(offer(1));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unsupported(MessageType::OFFER_CHANNEL))
    );
    host.send(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unsupported(MessageType::ALL_OFFERS_DELIVERED))
    );
    // And an answer nobody asked for is no better.
    host.send(OutgoingMessage::new(&protocol::VersionResponse {
        version_supported: 1,
        connection_state: protocol::ConnectionState::SUCCESSFUL,
        padding: 0,
        selected_version_or_connection_id: 9,
    }));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unexpected(MessageType::VERSION_RESPONSE))
    );
    assert_eq!(client.state(), ClientState::Disconnected);

    client.connect().unwrap();
    let events = drain(&mut client, &mut host);
    assert!(matches!(events[..], [Event::Connected { .. }]));
}

#[test]
fn an_answer_to_contact_out_of_place_is_refused() {
    let (mut client, mut host) = connected();
    for answer in [
        OutgoingMessage::new(&protocol::VersionResponse {
            version_supported: 0,
            connection_state: protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 0,
        }),
        OutgoingMessage::new(&protocol::VersionResponse2 {
            version_response: protocol::VersionResponse {
                version_supported: 1,
                connection_state: protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 7,
            },
            supported_features: 0,
        }),
        OutgoingMessage::new(&protocol::VersionResponse3 {
            version_response2: protocol::VersionResponse2 {
                version_response: protocol::VersionResponse {
                    version_supported: 1,
                    connection_state: protocol::ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: 7,
                },
                supported_features: 0,
            },
            _padding: 0,
            parent_to_child_monitor_page_gpa: 0,
            child_to_parent_monitor_page_gpa: 0,
        }),
    ] {
        host.send(answer);
        assert_eq!(
            client.poll(&mut host),
            Err(HostError::Unexpected(MessageType::VERSION_RESPONSE))
        );
    }
    assert_eq!(client.state(), ClientState::Connected);
    still_works(&mut client, &mut host);
}

#[test]
fn a_version_accepted_with_no_connection_to_use_is_refused() {
    // A host that answers contact itself, wrongly: 6.0 accepted, connection ID zero.
    struct NoConnection(FakeHost);
    impl moruna_vmbus::Hv for NoConnection {
        fn post_message(
            &mut self,
            _: u32,
            _: u32,
            _: &[u8],
        ) -> Result<(), moruna_vmbus::PostError> {
            self.0
                .send(OutgoingMessage::new(&protocol::VersionResponse2 {
                    version_response: protocol::VersionResponse {
                        version_supported: 1,
                        connection_state: protocol::ConnectionState::SUCCESSFUL,
                        padding: 0,
                        selected_version_or_connection_id: 0,
                    },
                    supported_features: 0,
                }));
            Ok(())
        }
        fn signal_event(&mut self, connection_id: u32, flag: u16) {
            self.0.signal_event(connection_id, flag)
        }
        fn take_message(&mut self) -> Option<HvMessage> {
            self.0.take_message()
        }
        fn test_and_clear_event(&mut self, flag: u16) -> bool {
            self.0.test_and_clear_event(flag)
        }
        fn page_numbers(&mut self, memory: moruna_vmbus::MemoryId) -> Option<Vec<u64>> {
            self.0.page_numbers(memory)
        }
    }

    let mut host = NoConnection(FakeHost::new());
    let mut client = Client::new(0);
    client.connect().unwrap();
    assert_eq!(client.poll(&mut host), Err(HostError::ConnectionId(0)));
    assert_eq!(client.state(), ClientState::Connecting);
    assert_eq!(client.version(), None);
}

#[test]
fn a_message_only_a_guest_sends_is_refused() {
    let (mut client, mut host) = connected();
    let zeroes = [0u8; 232];
    for (typ, size) in [
        (MessageType::REQUEST_OFFERS, 0),
        (MessageType::OPEN_CHANNEL, 140),
        (MessageType::OPEN_CHANNEL, 148),
        (MessageType::CLOSE_CHANNEL, 4),
        (MessageType::GPADL_HEADER, 12),
        (MessageType::GPADL_BODY, 8),
        (MessageType::GPADL_TEARDOWN, 8),
        (MessageType::REL_ID_RELEASED, 4),
        (MessageType::INITIATE_CONTACT, 32),
        (MessageType::INITIATE_CONTACT, 48),
        (MessageType::UNLOAD, 0),
        (MessageType::OPEN_RESERVED_CHANNEL, 20),
        (MessageType::CLOSE_RESERVED_CHANNEL, 12),
        (MessageType::TL_CONNECT_REQUEST, 32),
        (MessageType::TL_CONNECT_REQUEST, 48),
        (MessageType::MODIFY_CHANNEL, 8),
    ] {
        refused(
            &mut client,
            &mut host,
            &raw(typ.0, &zeroes[..size]),
            HostError::NotForGuest(typ),
        );
    }
    still_works(&mut client, &mut host);
}

#[test]
fn an_answer_to_a_request_the_client_never_makes_is_refused() {
    let (mut client, mut host) = connected();
    for (typ, size) in [
        (MessageType::MODIFY_CHANNEL_RESPONSE, 8),
        (MessageType::TL_CONNECT_REQUEST_RESULT, 36),
        (MessageType::CLOSE_RESERVED_RESPONSE, 4),
    ] {
        refused(
            &mut client,
            &mut host,
            &raw(typ.0, &[0; 36][..size]),
            HostError::NotForGuest(typ),
        );
    }
    // These five exist only with features the client did not ask for, so they are not
    // messages of this connection at all.
    for typ in [
        MessageType::MODIFY_CONNECTION,
        MessageType::MODIFY_CONNECTION_RESPONSE,
        MessageType::PAUSE,
        MessageType::PAUSE_RESPONSE,
        MessageType::RESUME,
    ] {
        refused(
            &mut client,
            &mut host,
            &raw(typ.0, &[0; 16]),
            HostError::Unsupported(typ),
        );
    }
    still_works(&mut client, &mut host);
}

#[test]
fn a_message_newer_than_the_version_is_refused() {
    // At 4.0 the answer to a channel's modification does not exist yet.
    let mut host = FakeHost::up_to(Version::Win10);
    host.devices = vec![(NIC, instance(1))];
    let (mut client, _) = crate::host::connect(&mut host);
    refused(
        &mut client,
        &mut host,
        &raw(MessageType::MODIFY_CHANNEL_RESPONSE.0, &[0; 8]),
        HostError::Unsupported(MessageType::MODIFY_CHANNEL_RESPONSE),
    );
    still_works(&mut client, &mut host);
}

#[test]
fn an_offer_out_of_place_is_refused() {
    let (mut client, mut host) = connected();
    // Channel IDs are 1 to 2,047.
    for id in [0, 2048, u32::MAX] {
        host.send(offer(id));
        assert_eq!(
            client.poll(&mut host),
            Err(HostError::ChannelId(ChannelId(id)))
        );
    }
    // The largest there is, once.
    host.send(offer(2047));
    assert!(matches!(client.poll(&mut host), Ok(Some(Event::Offer(_)))));
    host.send(offer(2047));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::DuplicateOffer(ChannelId(2047)))
    );
    host.send(offer(1));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::DuplicateOffer(CHANNEL))
    );
    // The first offers end once.
    host.send(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unexpected(MessageType::ALL_OFFERS_DELIVERED))
    );
    assert_eq!(client.offers().count(), 3);
    still_works(&mut client, &mut host);
}

#[test]
fn a_message_about_a_channel_the_host_never_offered_is_refused() {
    let (mut client, mut host) = connected();
    let unknown = ChannelId(40);
    host.send(OutgoingMessage::new(&protocol::RescindChannelOffer {
        channel_id: unknown,
    }));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::UnknownChannel(unknown))
    );
    host.send(OutgoingMessage::new(&protocol::GpadlCreated {
        channel_id: unknown,
        gpadl_id: GpadlId(1),
        status: 0,
    }));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::UnknownChannel(unknown))
    );
    host.send(OutgoingMessage::new(&protocol::OpenResult {
        channel_id: unknown,
        open_id: 0,
        status: 0,
    }));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::UnknownChannel(unknown))
    );
    still_works(&mut client, &mut host);
}

#[test]
fn an_answer_about_a_gpadl_nobody_asked_about_is_refused() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 2);
    let created = |gpadl| {
        OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: CHANNEL,
            gpadl_id: gpadl,
            status: 0,
        })
    };
    let torndown = |gpadl| OutgoingMessage::new(&protocol::GpadlTorndown { gpadl_id: gpadl });

    // No such GPADL.
    host.send(created(GpadlId(5)));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(GpadlId(5))));
    host.send(torndown(GpadlId(5)));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(GpadlId(5))));

    // A GPADL the host has: created a second time, and torn down unasked.
    let gpadl = client.create_gpadl(&mut host, CHANNEL, memory).unwrap();
    drain(&mut client, &mut host);
    host.send(created(gpadl));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(gpadl)));
    host.send(torndown(gpadl));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(gpadl)));
    // Created on another channel than its own.
    host.send(OutgoingMessage::new(&protocol::GpadlCreated {
        channel_id: ChannelId(2),
        gpadl_id: gpadl,
        status: 0,
    }));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(gpadl)));

    // The GPADL is untouched by all of it.
    client.teardown_gpadl(CHANNEL, gpadl).unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::GpadlTorndown {
            channel: CHANNEL,
            gpadl
        }]
    );
    // And torn down a second time.
    host.send(torndown(gpadl));
    assert_eq!(client.poll(&mut host), Err(HostError::Gpadl(gpadl)));
    still_works(&mut client, &mut host);
}

#[test]
fn an_open_result_nobody_asked_for_is_refused() {
    let (mut client, mut host) = connected();
    let result = |open_id, status| {
        OutgoingMessage::new(&protocol::OpenResult {
            channel_id: CHANNEL,
            open_id,
            status,
        })
    };
    // For a channel that is not opening.
    host.send(result(0, 0));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unexpected(MessageType::OPEN_CHANNEL_RESULT))
    );
    assert!(!client.is_open(CHANNEL));

    // For another open than the one asked.
    let memory = host.add_memory(1, 0x1000, 4);
    let ring = RingPair {
        memory,
        outgoing_pages: 2,
    };
    host.hold_open = true;
    client.open(&mut host, CHANNEL, ring).unwrap();
    drain(&mut client, &mut host);
    host.send(result(3, 0));
    assert_eq!(client.poll(&mut host), Err(HostError::OpenId(3)));
    assert!(!client.is_open(CHANNEL));

    // The real answer still opens it, and a second one is refused.
    host.release();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Opened { channel: CHANNEL }]
    );
    host.send(result(0, 0));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unexpected(MessageType::OPEN_CHANNEL_RESULT))
    );
    assert!(client.is_open(CHANNEL));
}

#[test]
fn an_unload_nobody_asked_for_is_refused() {
    let (mut client, mut host) = connected();
    host.send(OutgoingMessage::new(&protocol::UnloadComplete {}));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unexpected(MessageType::UNLOAD_COMPLETE))
    );
    assert_eq!(client.state(), ClientState::Connected);
    assert_eq!(client.offers().count(), 2);
    still_works(&mut client, &mut host);
}

#[test]
fn garbage_does_not_hold_up_what_follows_it() {
    let (mut client, mut host) = connected();
    // Garbage, a real offer, garbage, a real rescind: both real ones arrive.
    host.send_bytes(&raw(77, &[]));
    let added = host.offer(NIC, instance(5));
    host.send_bytes(&[]);
    host.rescind(ChannelId(2));

    let mut events = Vec::new();
    let mut errors = Vec::new();
    loop {
        match client.poll(&mut host) {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break,
            Err(error) => errors.push(error),
        }
    }
    assert_eq!(
        errors,
        [
            HostError::Unsupported(MessageType(77)),
            HostError::TooSmall(None)
        ]
    );
    assert!(matches!(
        events[..],
        [
            Event::Offer(offer),
            Event::Rescinded {
                channel: ChannelId(2)
            }
        ] if offer.channel == added
    ));
}

#[test]
fn while_unloading_the_hosts_other_answers_are_dropped_and_garbage_is_still_named() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 2);
    host.hold_gpadl = true;
    client.create_gpadl(&mut host, CHANNEL, memory).unwrap();
    drain(&mut client, &mut host);
    client.unload().unwrap();

    // The GPADL's answer and a late offer come before the host confirms the unload.
    host.release();
    host.send(offer(9));
    host.send_bytes(&raw(99, &[]));
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Unsupported(MessageType(99)))
    );
    assert_eq!(drain(&mut client, &mut host), [Event::Unloaded]);
    assert_eq!(client.offers().count(), 0);
    assert_eq!(
        client.create_gpadl(&mut host, CHANNEL, memory),
        Err(Error::NotConnected)
    );
}

#[test]
fn every_refusal_says_what_was_refused() {
    let host = [
        HostError::Post(moruna_vmbus::HvError::InvalidConnectionId),
        HostError::NotVmbus(2),
        HostError::Length(250),
        HostError::TooSmall(None),
        HostError::TooSmall(Some(MessageType::OFFER_CHANNEL)),
        HostError::Unsupported(MessageType(99)),
        HostError::NotForGuest(MessageType::UNLOAD),
        HostError::Unexpected(MessageType::VERSION_RESPONSE),
        HostError::ConnectionId(0),
        HostError::ChannelId(ChannelId(0)),
        HostError::DuplicateOffer(CHANNEL),
        HostError::UnknownChannel(CHANNEL),
        HostError::AlreadyRescinded(CHANNEL),
        HostError::Gpadl(GpadlId(3)),
        HostError::OpenId(4),
    ]
    .map(|error| error.to_string());
    let caller = [
        Error::NotDisconnected,
        Error::NotConnected,
        Error::UnknownChannel(CHANNEL),
        Error::Rescinded(CHANNEL),
        Error::NotClosed(CHANNEL),
        Error::NotOpen(CHANNEL),
        Error::UnknownMemory(moruna_vmbus::MemoryId(0xabc)),
        Error::PageCount(9000),
        Error::RingPages {
            pages: 3,
            outgoing: 2,
        },
        Error::UnknownGpadl(GpadlId(3)),
        Error::GpadlBusy(GpadlId(3)),
    ]
    .map(|error| error.to_string());

    let all: Vec<&String> = host.iter().chain(&caller).collect();
    for (i, text) in all.iter().enumerate() {
        assert!(!text.is_empty());
        assert!(all[..i].iter().all(|other| other != text), "{text}");
    }
    assert_eq!(caller[7], "a GPADL names 1 to 8190 pages, not 9000");
    assert_eq!(host[0], "Hyper-V refused a post: Invalid connection ID");
    assert_eq!(host[2], "a message of 250 bytes in a 240-byte slot");
    // Each is an error a caller can pass on.
    let _: &dyn core::error::Error = &HostError::OpenId(4);
    let _: &dyn core::error::Error = &Error::NotConnected;
}
