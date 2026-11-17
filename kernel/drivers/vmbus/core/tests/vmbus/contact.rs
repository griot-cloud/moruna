//! Contact and the protocol version, offers, a busy host, and unload.

use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::ClientState;
use moruna_vmbus::ConnectError;
use moruna_vmbus::ConnectionState;
use moruna_vmbus::Error;
use moruna_vmbus::Event;
use moruna_vmbus::FeatureFlags;
use moruna_vmbus::HostError;
use moruna_vmbus::HvError;
use moruna_vmbus::MemoryId;
use moruna_vmbus::MessageType;
use moruna_vmbus::Offer;
use moruna_vmbus::RingPair;
use moruna_vmbus::VERSIONS;
use moruna_vmbus::Version;
use moruna_vmbus::VersionInfo;

use crate::host::FakeHost;
use crate::host::NAMED_CONNECTION;
use crate::host::NIC;
use crate::host::SCSI;
use crate::host::connect;
use crate::host::connected;
use crate::host::connection_of;
use crate::host::drain;
use crate::host::instance;

fn plain(version: Version) -> VersionInfo {
    VersionInfo {
        version,
        feature_flags: FeatureFlags::new(),
    }
}

#[test]
fn the_newest_version_is_offered_first_and_accepted() {
    let mut host = FakeHost::new();
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::Connected {
            version: plain(Version::Copper)
        }]
    );
    assert_eq!(host.asked, [Version::Copper as u32]);
    // Contact goes to connection 4; what follows, to the connection the host named.
    assert_eq!(
        host.posted,
        [
            (4, MessageType::INITIATE_CONTACT),
            (NAMED_CONNECTION, MessageType::REQUEST_OFFERS),
        ]
    );
    assert_eq!(client.state(), ClientState::Connected);
    assert_eq!(client.version(), Some(plain(Version::Copper)));
    assert!(!client.has_unsent());
}

#[test]
fn the_versions_offered_are_the_newest_down_to_four_point_zero() {
    // The newest `vmbus_core` defines comes first, each is older than the one before,
    // and the last is Windows 10's.
    assert_eq!(VERSIONS[0], Version::Copper);
    assert!(VERSIONS.windows(2).all(|pair| pair[0] > pair[1]));
    assert_eq!(VERSIONS[VERSIONS.len() - 1], Version::Win10);
    assert_eq!(Version::Win10 as u32, 4 << 16);
}

#[test]
fn a_refused_version_steps_down_to_the_next() {
    let mut host = FakeHost::up_to(Version::Win10Rs5);
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::Connected {
            version: plain(Version::Win10Rs5)
        }]
    );
    assert_eq!(
        host.asked,
        [
            Version::Copper as u32,
            Version::Iron as u32,
            Version::Win10Rs5 as u32
        ]
    );
    assert_eq!(client.version(), Some(plain(Version::Win10Rs5)));
}

#[test]
fn a_version_before_five_uses_connection_one_throughout() {
    let mut host = FakeHost::up_to(Version::Win10Rs3_0);
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::Connected {
            version: plain(Version::Win10Rs3_0)
        }]
    );
    // Five refusals on connection 4, then 4.1 asked and everything after it on 1.
    let connections: Vec<u32> = host.posted.iter().map(|(id, _)| *id).collect();
    assert_eq!(connections, [4, 4, 4, 4, 4, 1, 1]);
    assert_eq!(client.version(), Some(plain(Version::Win10Rs3_0)));
}

#[test]
fn four_point_zero_is_the_oldest_version_accepted() {
    let mut host = FakeHost::up_to(Version::Win10);
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::Connected {
            version: plain(Version::Win10)
        }]
    );
    let asked: Vec<u32> = VERSIONS.iter().map(|version| *version as u32).collect();
    assert_eq!(host.asked, asked);
    assert_eq!(client.state(), ClientState::Connected);
}

#[test]
fn a_host_older_than_four_point_zero_is_refused() {
    // Windows 8.1's host: it would take 3.0, and the client never asks for it.
    let mut host = FakeHost::up_to(Version::Win8_1);
    let (mut client, events) = connect(&mut host);

    assert_eq!(events, [Event::ConnectFailed(ConnectError::NoVersion)]);
    let asked: Vec<u32> = VERSIONS.iter().map(|version| *version as u32).collect();
    assert_eq!(host.asked, asked, "seven versions asked, none below 4.0");
    assert_eq!(client.state(), ClientState::Disconnected);
    assert_eq!(client.version(), None);
    assert!(!client.has_unsent());

    // The client may try again, and starts from the newest.
    host.versions.push(Version::Copper);
    client.connect().unwrap();
    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Connected {
            version: plain(Version::Copper)
        }]
    );
}

#[test]
fn a_host_that_knows_the_version_and_cannot_connect_fails_the_connection() {
    let mut host = FakeHost::new();
    host.connection_state = ConnectionState::FAILED_LOW_RESOURCES;
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::ConnectFailed(ConnectError::Refused(
            ConnectionState::FAILED_LOW_RESOURCES
        ))]
    );
    assert_eq!(
        host.asked,
        [Version::Copper as u32],
        "no older version is tried"
    );
    assert_eq!(client.state(), ClientState::Disconnected);
}

#[test]
fn features_the_client_did_not_ask_for_are_not_taken() {
    let mut host = FakeHost::new();
    host.features = FeatureFlags::from_bits(0xff);
    let (client, events) = connect(&mut host);

    assert_eq!(
        events,
        [Event::Connected {
            version: plain(Version::Copper)
        }]
    );
    assert_eq!(client.version(), Some(plain(Version::Copper)));
}

#[test]
fn each_offer_is_kept_with_its_interface_and_instance() {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1)), (SCSI, instance(2))];
    let (client, events) = connect(&mut host);

    let nic = Offer {
        channel: ChannelId(1),
        interface: NIC,
        instance: instance(1),
        subchannel: 0,
    };
    let disk = Offer {
        channel: ChannelId(2),
        interface: SCSI,
        instance: instance(2),
        subchannel: 0,
    };
    assert_eq!(
        events,
        [
            Event::Offer(nic),
            Event::Offer(disk),
            Event::Connected {
                version: plain(Version::Copper)
            }
        ]
    );
    assert_eq!(client.offers().collect::<Vec<_>>(), [nic, disk]);

    // The whole offer is kept too: the connection ID the channel signals on is in it.
    let whole = client.offer(ChannelId(2)).unwrap();
    assert_eq!(whole.interface_id, SCSI);
    assert_eq!(whole.instance_id, instance(2));
    assert_eq!(whole.connection_id, connection_of(ChannelId(2)));
    assert!(client.offer(ChannelId(3)).is_none());
}

#[test]
fn a_device_added_later_is_offered_then() {
    let (mut client, mut host) = connected();
    let channel = host.offer(SCSI, instance(9));

    assert_eq!(
        drain(&mut client, &mut host),
        [Event::Offer(Offer {
            channel,
            interface: SCSI,
            instance: instance(9),
            subchannel: 0,
        })]
    );
    assert_eq!(client.offers().count(), 3);
    assert_eq!(client.state(), ClientState::Connected);
}

#[test]
fn connect_is_refused_unless_disconnected() {
    let mut host = FakeHost::new();
    let mut client = Client::new(0);
    client.connect().unwrap();
    assert_eq!(client.state(), ClientState::Connecting);
    assert_eq!(client.connect(), Err(Error::NotDisconnected));

    drain(&mut client, &mut host);
    assert_eq!(client.connect(), Err(Error::NotDisconnected));
    assert_eq!(host.asked.len(), 1, "the second connect sent nothing");
}

#[test]
fn a_busy_post_is_retried_until_the_host_takes_it() {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1))];
    host.busy = 3;
    let mut client = Client::new(0);
    client.connect().unwrap();

    // Three polls find the host busy: nothing is lost and nothing is sent twice.
    for refused in 1..=3 {
        assert_eq!(client.poll(&mut host), Ok(None));
        assert!(client.has_unsent());
        assert_eq!(host.refused, refused);
        assert!(host.posted.is_empty());
        assert_eq!(client.state(), ClientState::Connecting);
    }

    // The fourth finds room.
    let events = drain(&mut client, &mut host);
    assert!(matches!(
        events[..],
        [Event::Offer(_), Event::Connected { .. }]
    ));
    assert_eq!(
        host.types(),
        [MessageType::INITIATE_CONTACT, MessageType::REQUEST_OFFERS]
    );
    assert!(!client.has_unsent());
}

#[test]
fn a_post_hyper_v_refuses_is_an_error_and_the_message_waits_first_in_line() {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1))];
    host.refuse_post = Some(HvError::InvalidConnectionId);
    let mut client = Client::new(0);
    client.connect().unwrap();

    // Nothing was posted, and the client says why, with Hyper-V's status.
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Post(HvError::InvalidConnectionId))
    );
    assert!(host.posted.is_empty());
    assert!(client.has_unsent());
    assert_eq!(client.state(), ClientState::Connecting);

    // A caller that polls again posts the same message, once.
    let events = drain(&mut client, &mut host);
    assert!(matches!(
        events[..],
        [Event::Offer(_), Event::Connected { .. }]
    ));
    assert_eq!(
        host.types(),
        [MessageType::INITIATE_CONTACT, MessageType::REQUEST_OFFERS]
    );
}

#[test]
fn a_post_refused_later_keeps_the_messages_after_it_in_order() {
    let (mut client, mut host) = connected();
    let memory = host.add_memory(1, 0x1000, 100);
    client
        .create_gpadl(&mut host, ChannelId(1), memory)
        .unwrap();
    host.refuse_post = Some(HvError::InsufficientMemory);

    // The header goes nowhere; the body after it is not posted ahead of it.
    assert_eq!(
        client.poll(&mut host),
        Err(HostError::Post(HvError::InsufficientMemory))
    );
    assert_eq!(
        host.types().len(),
        2,
        "only contact and the request for offers"
    );
    let events = drain(&mut client, &mut host);
    assert!(matches!(events[..], [Event::GpadlCreated { .. }]));
    assert_eq!(
        host.types()[2..],
        [
            MessageType::GPADL_HEADER,
            MessageType::GPADL_BODY,
            MessageType::GPADL_BODY,
            MessageType::GPADL_BODY
        ]
    );
}

#[test]
fn a_host_that_turns_busy_mid_way_gets_every_message_once_and_in_order() {
    let (mut client, mut host) = connected();
    // 200 pages: a header and seven bodies. The host takes two, then is busy five times.
    let ring = host.add_memory(1, 0x1000, 200);
    client
        .open(
            &mut host,
            ChannelId(1),
            RingPair {
                memory: ring,
                outgoing_pages: 100,
            },
        )
        .unwrap();

    host.busy_at = Some((host.posted.len() + 2, 5));
    let mut events = Vec::new();
    let mut polls = 0;
    loop {
        events.extend(drain(&mut client, &mut host));
        polls += 1;
        if !client.has_unsent() {
            break;
        }
    }
    assert_eq!(
        events,
        [Event::Opened {
            channel: ChannelId(1)
        }]
    );
    assert_eq!(host.refused, 5);
    assert!(polls > 5);
    // The strict host checked the order; this checks nothing was sent twice.
    let headers = host
        .types()
        .iter()
        .filter(|typ| **typ == MessageType::GPADL_HEADER)
        .count();
    let bodies = host
        .types()
        .iter()
        .filter(|typ| **typ == MessageType::GPADL_BODY)
        .count();
    assert_eq!((headers, bodies), (1, 7));
    let pages: Vec<u64> = (0x1000..0x1000 + 200).collect();
    assert_eq!(
        host.gpadl_pages(host.ring_gpadl(ChannelId(1)).unwrap()),
        Some(pages)
    );
}

#[test]
fn unload_ends_the_connection_and_forgets_everything() {
    let (mut client, mut host) = connected();
    let ring = host.add_memory(1, 0x1000, 8);
    client
        .open(
            &mut host,
            ChannelId(1),
            RingPair {
                memory: ring,
                outgoing_pages: 4,
            },
        )
        .unwrap();
    drain(&mut client, &mut host);
    assert!(client.is_open(ChannelId(1)));

    client.unload().unwrap();
    assert_eq!(client.state(), ClientState::Disconnecting);
    assert_eq!(client.version(), Some(plain(Version::Copper)));
    // Nothing else is taken while the host has not confirmed.
    assert_eq!(client.unload(), Err(Error::NotConnected));
    assert_eq!(client.close(ChannelId(1)), Err(Error::NotConnected));

    assert_eq!(drain(&mut client, &mut host), [Event::Unloaded]);
    assert_eq!(host.types().last(), Some(&MessageType::UNLOAD));
    assert_eq!(client.state(), ClientState::Disconnected);
    assert_eq!(client.version(), None);
    assert_eq!(client.offers().count(), 0);
    assert!(!client.is_open(ChannelId(1)));
    assert!(!host.knows(ChannelId(1)));

    // And the client can make contact again.
    client.connect().unwrap();
    let events = drain(&mut client, &mut host);
    assert!(matches!(events.last(), Some(Event::Connected { .. })));
    assert_eq!(client.offers().count(), 2);
}

#[test]
fn unload_while_the_offers_arrive_drops_the_rest_of_them() {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1)), (SCSI, instance(2))];
    let mut client = Client::new(0);
    client.connect().unwrap();

    // The first offer has come; the second and the end of the offers are still waiting.
    assert!(matches!(client.poll(&mut host), Ok(Some(Event::Offer(_)))));
    assert_eq!(client.state(), ClientState::RequestingOffers);
    client.unload().unwrap();

    assert_eq!(drain(&mut client, &mut host), [Event::Unloaded]);
    assert_eq!(client.state(), ClientState::Disconnected);
    assert_eq!(client.offers().count(), 0);
}

#[test]
fn every_request_needs_a_connection() {
    let mut host = FakeHost::new();
    let memory = host.add_memory(1, 0x1000, 8);
    let ring = RingPair {
        memory,
        outgoing_pages: 4,
    };
    let channel = ChannelId(1);
    let mut client = Client::new(0);

    let check = |client: &mut Client, host: &mut FakeHost| {
        assert_eq!(client.open(host, channel, ring), Err(Error::NotConnected));
        assert_eq!(client.close(channel), Err(Error::NotConnected));
        assert_eq!(
            client.create_gpadl(host, channel, MemoryId(1)),
            Err(Error::NotConnected)
        );
        assert_eq!(
            client.teardown_gpadl(channel, moruna_vmbus::GpadlId(1)),
            Err(Error::NotConnected)
        );
        assert_eq!(client.signal_host(host, channel), Err(Error::NotConnected));
        assert!(client.take_signals(host).is_empty());
        assert!(host.posted.is_empty() && host.signals.is_empty());
    };

    // Disconnected, and connecting.
    check(&mut client, &mut host);
    assert_eq!(client.unload(), Err(Error::NotConnected));
    client.connect().unwrap();
    check(&mut client, &mut host);
    assert_eq!(client.unload(), Err(Error::NotConnected));
}
