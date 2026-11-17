//! The fake host is strict: each rule it keeps, broken here on purpose, is a panic that
//! names the rule. So a test of the client that passes has broken none of them.

use moruna_vmbus::ChannelId;
use moruna_vmbus::GpadlId;
use moruna_vmbus::Hv;
use moruna_vmbus::Version;
use vmbus_core::OutgoingMessage;
use vmbus_core::protocol;

use crate::host::FakeHost;
use crate::host::NAMED_CONNECTION;
use crate::host::NIC;
use crate::host::instance;

fn post(host: &mut FakeHost, connection: u32, message: OutgoingMessage) {
    host.post_message(connection, 1, message.data())
        .expect("the host is not busy");
}

fn contact(version: Version) -> OutgoingMessage {
    let target = protocol::TargetInfo::new().with_sint(2).into_bits();
    OutgoingMessage::new(&protocol::InitiateContact {
        version_requested: version as u32,
        target_message_vp: 0,
        interrupt_page_or_target_info: target,
        parent_to_child_monitor_page_gpa: 0,
        child_to_parent_monitor_page_gpa: 0,
    })
}

/// A host in contact at 6.0 that has offered one channel, 1.
fn offered() -> FakeHost {
    let mut host = FakeHost::new();
    host.devices = vec![(NIC, instance(1))];
    post(&mut host, 4, contact(Version::Copper));
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::RequestOffers {}),
    );
    host
}

/// A GPADL of four pages on channel 1, whole.
fn ring_gpadl(host: &mut FakeHost, id: u32) {
    let values: Vec<u8> = [4 * 4096u64, 10, 11, 12, 13]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect();
    post(
        host,
        NAMED_CONNECTION,
        OutgoingMessage::with_data(
            &protocol::GpadlHeader {
                channel_id: ChannelId(1),
                gpadl_id: GpadlId(id),
                len: 40,
                count: 1,
            },
            &values,
        ),
    );
}

fn open(target_vp: u32, ring: u32, split: u32) -> OutgoingMessage {
    OutgoingMessage::new(&protocol::OpenChannel {
        channel_id: ChannelId(1),
        open_id: 0,
        ring_buffer_gpadl_id: GpadlId(ring),
        target_vp,
        downstream_ring_buffer_page_offset: split,
        user_data: protocol::UserDefinedData::default(),
    })
}

#[test]
fn a_guest_that_keeps_the_rules_passes() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    post(&mut host, NAMED_CONNECTION, open(0, 1, 2));
    assert!(host.is_open(ChannelId(1)));
}

#[test]
#[should_panic(expected = "SynIC type 1")]
fn another_synic_message_type() {
    let mut host = FakeHost::new();
    let message = contact(Version::Copper);
    let _ = host.post_message(4, 2, message.data());
}

#[test]
#[should_panic(expected = "the host cannot read the guest's message")]
fn a_request_before_contact() {
    let mut host = FakeHost::new();
    post(
        &mut host,
        1,
        OutgoingMessage::new(&protocol::RequestOffers {}),
    );
}

#[test]
#[should_panic(expected = "from 5.0, contact is asked on connection 4")]
fn contact_on_the_old_connection() {
    let mut host = FakeHost::new();
    post(&mut host, 1, contact(Version::Copper));
}

#[test]
#[should_panic(expected = "no version below 4.0 is asked")]
fn a_version_below_four_point_zero() {
    let mut host = FakeHost::new();
    post(&mut host, 1, contact(Version::Win8_1));
}

#[test]
#[should_panic(expected = "each version asked is older than the last")]
fn a_newer_version_after_an_older_one() {
    let mut host = FakeHost::up_to(Version::Win10);
    post(&mut host, 4, contact(Version::Iron));
    post(&mut host, 4, contact(Version::Copper));
}

#[test]
#[should_panic(expected = "contact is asked of a host that is not connected")]
fn contact_twice() {
    let mut host = FakeHost::new();
    post(&mut host, 4, contact(Version::Copper));
    post(&mut host, 4, contact(Version::Copper));
}

#[test]
#[should_panic(expected = "after contact, messages go to the connection the host named")]
fn a_message_to_another_connection_than_the_one_named() {
    let mut host = FakeHost::new();
    post(&mut host, 4, contact(Version::Copper));
    post(
        &mut host,
        4,
        OutgoingMessage::new(&protocol::RequestOffers {}),
    );
}

#[test]
#[should_panic(expected = "no bytes after the fields")]
fn bytes_after_a_messages_fields() {
    let mut host = FakeHost::new();
    post(&mut host, 4, contact(Version::Copper));
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::with_data(&protocol::RequestOffers {}, &[0; 8]),
    );
}

#[test]
#[should_panic(expected = "offers are asked once")]
fn offers_asked_twice() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::RequestOffers {}),
    );
}

#[test]
#[should_panic(expected = "the header carries as many values as fit")]
fn a_gpadl_header_with_fewer_values_than_it_promises() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::with_data(
            &protocol::GpadlHeader {
                channel_id: ChannelId(1),
                gpadl_id: GpadlId(1),
                len: 16,
                count: 1,
            },
            &4096u64.to_le_bytes(),
        ),
    );
}

#[test]
#[should_panic(expected = "a body continues a GPADL whose header came first")]
fn a_gpadl_body_with_no_header() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::with_data(
            &protocol::GpadlBody {
                rsvd: 0,
                gpadl_id: GpadlId(1),
            },
            &[0; 8],
        ),
    );
}

#[test]
#[should_panic(expected = "a GPADL ID is used once at a time")]
fn a_gpadl_id_used_twice() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    ring_gpadl(&mut host, 1);
}

#[test]
#[should_panic(expected = "every channel targets CPU 0")]
fn an_open_that_targets_another_cpu() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    post(&mut host, NAMED_CONNECTION, open(1, 1, 2));
}

#[test]
#[should_panic(expected = "the ring's GPADL is created before the open")]
fn an_open_before_its_rings() {
    let mut host = offered();
    post(&mut host, NAMED_CONNECTION, open(0, 1, 2));
}

#[test]
#[should_panic(expected = "each ring has a control page and data")]
fn an_open_that_splits_the_rings_at_the_edge() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    post(&mut host, NAMED_CONNECTION, open(0, 1, 3));
}

#[test]
#[should_panic(expected = "an open is asked of a closed channel")]
fn an_open_of_an_open_channel() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    post(&mut host, NAMED_CONNECTION, open(0, 1, 2));
    post(&mut host, NAMED_CONNECTION, open(0, 1, 2));
}

#[test]
#[should_panic(expected = "a close is asked of an open channel")]
fn a_close_of_a_closed_channel() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::CloseChannel {
            channel_id: ChannelId(1),
        }),
    );
}

#[test]
#[should_panic(expected = "the ring's GPADL is torn down after the channel is closed")]
fn the_rings_torn_down_under_an_open_channel() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    post(&mut host, NAMED_CONNECTION, open(0, 1, 2));
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::GpadlTeardown {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
        }),
    );
}

#[test]
#[should_panic(expected = "only a rescinded channel is released")]
fn a_release_of_an_offer_that_stands() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::RelIdReleased {
            channel_id: ChannelId(1),
        }),
    );
}

#[test]
#[should_panic(expected = "a channel is released with none of its GPADLs left")]
fn a_release_that_leaves_a_gpadl_behind() {
    let mut host = offered();
    ring_gpadl(&mut host, 1);
    host.rescind(ChannelId(1));
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::RelIdReleased {
            channel_id: ChannelId(1),
        }),
    );
}

#[test]
#[should_panic(expected = "the client sends no such message")]
fn a_message_the_client_never_sends() {
    let mut host = offered();
    post(
        &mut host,
        NAMED_CONNECTION,
        OutgoingMessage::new(&protocol::ModifyChannel {
            channel_id: ChannelId(1),
            target_vp: 0,
        }),
    );
}
