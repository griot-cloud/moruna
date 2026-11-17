//! The in-memory channel (`testing`): a driver written against `Channel` alone, with a
//! fake device on the other end.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use guestmem_core::MemoryRead;
use guestmem_core::MemoryWrite;
use guestmem_core::ranges::PagedRange;
use moruna_vmbus::Channel;
use moruna_vmbus::ChannelError;
use moruna_vmbus::GpadlId;
use moruna_vmbus::MemoryId;
use moruna_vmbus::Wait;
use moruna_vmbus::testing::Device;
use moruna_vmbus::testing::Host;
use moruna_vmbus::testing::Memory;
use moruna_vmbus::testing::TestChannel;
use vmbus_ring::IncomingPacketType;
use vmbus_ring::OutgoingPacket;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::ReadError;
use vmbus_ring::Ring;
use vmbus_ring::RingMem;
use vmbus_ring::TransferPageRange;
use vmbus_ring::WriteError;

/// A device that answers every packet that asks for an answer with its payload
/// reversed, and has things to say unasked when the test gives it some.
#[derive(Default)]
struct Echo {
    seen: Vec<Vec<u8>>,
    kinds: Vec<&'static str>,
    ranges: Vec<Vec<u8>>,
    unasked: VecDeque<Vec<u8>>,
    shared: Vec<GpadlId>,
    refuse_memory: bool,
    leave_when_idle: bool,
}

impl Device for Echo {
    fn signalled(&mut self, host: &mut Host) {
        while let Some(received) = host.read().expect("the driver's ring is sound") {
            self.kinds.push(match received.packet.typ {
                IncomingPacketType::InBand => "in band",
                IncomingPacketType::Completion => "completion",
                IncomingPacketType::GpaDirect(..) => "GPA direct",
                IncomingPacketType::TransferPages(..) => "transfer pages",
            });
            self.ranges.push(received.ranges);
            if let Some(id) = received.packet.transaction_id {
                let answer: Vec<u8> = received.payload.iter().rev().copied().collect();
                host.write(OutgoingPacketType::Completion, id, &answer)
                    .expect("room for the answer");
            }
            self.seen.push(received.payload);
        }
    }

    fn idle(&mut self, host: &mut Host) {
        if self.leave_when_idle {
            host.rescind();
        } else if let Some(packet) = self.unasked.pop_front() {
            host.write(OutgoingPacketType::InBandNoCompletion, 0, &packet)
                .expect("room for the packet");
        }
    }

    fn shared(&mut self, gpadl: GpadlId, memory: &Memory) -> bool {
        if self.refuse_memory {
            return false;
        }
        memory.write(0, b"the host was here");
        self.shared.push(gpadl);
        true
    }
}

/// A device that never answers.
struct Mute;

impl Device for Mute {
    fn signalled(&mut self, _host: &mut Host) {}
}

fn channel() -> TestChannel<Echo> {
    TestChannel::new(4096, 4096, Echo::default()).expect("whole pages")
}

/// A driver's send, against the trait alone: write the packet, and signal the host if
/// the ring says to.
fn send<C: Channel>(
    channel: &mut C,
    typ: OutgoingPacketType<'_>,
    transaction_id: u64,
    payload: &[u8],
) -> Result<(), WriteError> {
    let ring = channel.outgoing();
    let mut offset = ring.outgoing()?;
    let packet = OutgoingPacket {
        transaction_id,
        size: payload.len(),
        typ,
    };
    let range = ring.write(&mut offset, &packet)?;
    range
        .writer(ring)
        .write(payload)
        .expect("the range is the payload's");
    if ring.commit_write(&mut offset) {
        channel.signal_host().expect("an open channel");
    }
    Ok(())
}

/// A driver's receive, against the trait alone.
fn receive<C: Channel>(channel: &mut C) -> Option<(Option<u64>, IncomingPacketType, Vec<u8>)> {
    let ring = channel.incoming();
    let mut offset = ring.incoming().expect("a sound ring");
    let packet = match ring.read(&mut offset) {
        Ok(packet) => packet,
        Err(ReadError::Empty) => return None,
        Err(ReadError::Corrupt(error)) => panic!("{error}"),
    };
    let payload = packet.payload.reader(ring).read_all().expect("in the ring");
    if ring.commit_read(&mut offset) {
        channel.signal_host().expect("an open channel");
    }
    Some((packet.transaction_id, packet.typ, payload))
}

#[test]
fn a_packet_reaches_the_device_and_its_answer_comes_back() {
    let mut channel = channel();
    channel.incoming().set_interrupt_mask(false);
    send(
        &mut channel,
        OutgoingPacketType::InBandWithCompletion,
        42,
        b"12345678",
    )
    .unwrap();

    // The host was idle, so the ring asked for a signal, once.
    assert_eq!(channel.host_signals(), 1);
    assert_eq!(channel.device().seen, [b"12345678"]);
    assert_eq!(channel.device().kinds, ["in band"]);

    assert_eq!(channel.wait(None), Ok(Wait::Signalled));
    let (id, typ, payload) = receive(&mut channel).unwrap();
    assert_eq!(id, Some(42));
    assert!(matches!(typ, IncomingPacketType::Completion));
    assert_eq!(payload, b"87654321");
    assert!(receive(&mut channel).is_none());
    // The signal was taken by the wait.
    assert_eq!(channel.wait(Some(1_000_000)), Ok(Wait::TimedOut));
}

#[test]
fn a_packet_that_asks_for_no_answer_gets_none() {
    let mut channel = channel();
    channel.incoming().set_interrupt_mask(false);
    send(
        &mut channel,
        OutgoingPacketType::InBandNoCompletion,
        7,
        b"quietly!",
    )
    .unwrap();
    assert_eq!(channel.device().seen, [b"quietly!"]);
    assert_eq!(channel.wait(Some(0)), Ok(Wait::TimedOut));
    assert!(receive(&mut channel).is_none());
}

#[test]
fn a_payload_is_padded_to_eight_bytes_in_the_ring() {
    // The ring's packets are whole 8-byte words: a driver's own header says how much of
    // the payload is its.
    let mut channel = channel();
    send(
        &mut channel,
        OutgoingPacketType::InBandNoCompletion,
        0,
        b"abc",
    )
    .unwrap();
    assert_eq!(channel.device().seen, [b"abc\0\0\0\0\0"]);
}

#[test]
fn a_driver_that_masks_its_ring_is_not_signalled() {
    let mut channel = channel();
    // A new channel's incoming ring is masked until the driver unmasks it.
    send(
        &mut channel,
        OutgoingPacketType::InBandWithCompletion,
        1,
        b"12345678",
    )
    .unwrap();
    assert_eq!(channel.wait(Some(0)), Ok(Wait::TimedOut));
    // The answer is there all the same, for a driver that looks.
    assert_eq!(receive(&mut channel).unwrap().2, b"87654321");
}

#[test]
fn the_device_speaks_unasked_when_the_driver_waits() {
    let mut channel = channel();
    channel.incoming().set_interrupt_mask(false);
    channel.device_mut().unasked.push_back(b"a frame!".to_vec());

    assert_eq!(channel.wait(Some(0)), Ok(Wait::Signalled));
    let (id, typ, payload) = receive(&mut channel).unwrap();
    assert_eq!(id, None);
    assert!(matches!(typ, IncomingPacketType::InBand));
    assert_eq!(payload, b"a frame!");
    assert_eq!(channel.host_signals(), 0);
    assert_eq!(channel.wait(Some(0)), Ok(Wait::TimedOut));
}

#[test]
fn a_test_can_be_the_device_between_the_drivers_calls() {
    let mut channel = TestChannel::new(8192, 8192, Mute).unwrap();
    channel.incoming().set_interrupt_mask(false);
    assert_eq!(channel.host().outgoing().maximum_packet_size(), 8192 - 8);
    assert!(
        channel
            .host()
            .incoming()
            .verify_interrupts_unmasked()
            .is_ok()
    );

    // Transfer pages: how a network card names a frame in its receive buffer.
    let ranges = [TransferPageRange {
        byte_count: 1500,
        byte_offset: 4096,
    }];
    channel
        .host()
        .write(
            OutgoingPacketType::TransferPages(3, &ranges),
            9,
            b"received",
        )
        .unwrap();
    assert_eq!(channel.wait(Some(0)), Ok(Wait::Signalled));
    let (id, typ, payload) = receive(&mut channel).unwrap();
    assert_eq!(id, Some(9));
    assert!(matches!(typ, IncomingPacketType::TransferPages(3, 1, _)));
    assert_eq!(payload, b"received");

    // A signal with nothing written.
    channel.host().signal_driver();
    assert_eq!(channel.wait(None), Ok(Wait::Signalled));

    // And the test reads what the driver sent to a device that reads nothing.
    send(&mut channel, OutgoingPacketType::Completion, 9, b"consumed").unwrap();
    assert_eq!(channel.host_signals(), 1);
    let received = channel.host().read().unwrap().unwrap();
    assert_eq!(received.packet.transaction_id, Some(9));
    assert_eq!(received.payload, b"consumed");
    assert!(received.ranges.is_empty());
    assert!(channel.host().read().unwrap().is_none());
}

#[test]
fn a_packet_that_names_memory_outside_the_ring_arrives_with_its_ranges() {
    let mut channel = channel();
    // GPA direct: how a disk driver names the pages of a read or a write.
    let pages = [77u64, 78];
    let range = PagedRange::new(512, 4096, &pages).unwrap();
    send(
        &mut channel,
        OutgoingPacketType::GpaDirect(&[range]),
        5,
        b"a request",
    )
    .unwrap();

    let device = channel.device();
    assert_eq!(device.kinds, ["GPA direct"]);
    assert_eq!(&device.seen[0][..9], b"a request");
    // One range: its length and offset, then its two page numbers.
    let mut expected = Vec::new();
    expected.extend(4096u32.to_le_bytes());
    expected.extend(512u32.to_le_bytes());
    expected.extend(77u64.to_le_bytes());
    expected.extend(78u64.to_le_bytes());
    assert_eq!(device.ranges, [expected]);

    // Transfer pages the other way round, which a driver does not send but the ring
    // allows: the device sees its ranges too.
    let ranges = [TransferPageRange {
        byte_count: 8,
        byte_offset: 16,
    }];
    send(
        &mut channel,
        OutgoingPacketType::TransferPages(1, &ranges),
        6,
        b"12345678",
    )
    .unwrap();
    assert_eq!(channel.device().kinds, ["GPA direct", "transfer pages"]);
    assert_eq!(channel.device().ranges[1].len(), 8);
}

#[test]
fn a_driver_waiting_for_room_is_told_when_the_device_makes_some() {
    let mut channel = TestChannel::new(4096, 4096, Mute).unwrap();
    // Three packets of 1,024 bytes with their headers fill what a 4,096-byte ring holds.
    let payload = [0x5a; 1000];
    for _ in 0..3 {
        send(
            &mut channel,
            OutgoingPacketType::InBandNoCompletion,
            0,
            &payload,
        )
        .unwrap();
    }
    assert!(matches!(
        send(
            &mut channel,
            OutgoingPacketType::InBandNoCompletion,
            0,
            &payload
        ),
        Err(WriteError::Full(1024))
    ));
    channel.outgoing().set_pending_send_size(1024).unwrap();
    assert_eq!(channel.wait(Some(0)), Ok(Wait::TimedOut));

    // The device reads one: there is room for the fourth, and the driver is told.
    assert!(channel.host().read().unwrap().is_some());
    assert_eq!(channel.wait(None), Ok(Wait::Signalled));
    send(
        &mut channel,
        OutgoingPacketType::InBandNoCompletion,
        0,
        &payload,
    )
    .unwrap();
}

#[test]
fn a_full_or_corrupt_ring_is_an_error_for_the_device_too() {
    let mut channel = TestChannel::new(4096, 4096, Mute).unwrap();
    let payload = [1; 1000];
    for _ in 0..3 {
        channel
            .host()
            .write(OutgoingPacketType::InBandNoCompletion, 0, &payload)
            .unwrap();
    }
    assert!(matches!(
        channel
            .host()
            .write(OutgoingPacketType::InBandNoCompletion, 0, &payload),
        Err(WriteError::Full(1024))
    ));

    // A driver that scribbles on its rings' control words: the pointers are no longer
    // offsets of packets.
    channel.incoming().mem().control()[1].store(3, Ordering::Relaxed);
    assert!(matches!(
        channel
            .host()
            .write(OutgoingPacketType::InBandNoCompletion, 0, b""),
        Err(WriteError::Corrupt(vmbus_ring::Error::InvalidRingPointer))
    ));
    channel.outgoing().mem().control()[0].store(4099, Ordering::Relaxed);
    assert!(matches!(
        channel.host().read(),
        Err(vmbus_ring::Error::InvalidRingPointer)
    ));
    // A pointer that is an offset, of something that is no packet.
    channel.outgoing().mem().control()[0].store(64, Ordering::Relaxed);
    assert!(matches!(
        channel.host().read(),
        Err(vmbus_ring::Error::InvalidDescriptorLengths)
    ));
}

#[test]
fn a_wait_with_a_deadline_and_nothing_to_wait_for_times_out() {
    let mut channel = channel();
    channel.incoming().set_interrupt_mask(false);
    assert_eq!(channel.wait(Some(0)), Ok(Wait::TimedOut));
    assert_eq!(channel.wait(Some(u64::MAX)), Ok(Wait::TimedOut));
}

#[test]
#[should_panic(expected = "the driver waits with no deadline")]
fn a_wait_with_no_deadline_that_could_never_end_fails_the_test() {
    let mut channel = channel();
    let _ = channel.wait(None);
}

#[test]
fn shared_memory_is_the_same_memory_on_both_ends() {
    let mut channel = channel();
    let buffer = Memory::new(2);
    assert_eq!(buffer.len(), 8192);
    assert!(!buffer.is_empty());
    assert_eq!(format!("{buffer:?}"), "Memory(8192 bytes)");
    channel.add_memory(MemoryId(5), buffer.clone());

    let gpadl = channel.share(MemoryId(5)).unwrap();
    assert_eq!(channel.device().shared, [gpadl]);

    // What the device wrote, the driver reads; what the driver writes, the device reads.
    let mut seen = [0; 17];
    buffer.read(0, &mut seen);
    assert_eq!(&seen, b"the host was here");
    buffer.write(8000, b"and the driver");
    let mut seen = [0; 14];
    channel.host().memory(gpadl).unwrap().read(8000, &mut seen);
    assert_eq!(&seen, b"and the driver");
    assert!(channel.host().memory(GpadlId(99)).is_none());

    // Shared again, it is another GPADL of the same memory.
    let again = channel.share(MemoryId(5)).unwrap();
    assert_ne!(again, gpadl);
    assert_eq!(channel.host().memory(again).unwrap().len(), 8192);
}

#[test]
fn memory_that_cannot_be_shared_is_refused() {
    let mut channel = channel();
    assert_eq!(channel.share(MemoryId(1)), Err(ChannelError::NotShared));

    let empty = Memory::new(0);
    assert!(empty.is_empty());
    channel.add_memory(MemoryId(2), empty);
    assert_eq!(channel.share(MemoryId(2)), Err(ChannelError::NotShared));

    channel.add_memory(MemoryId(3), Memory::new(1));
    channel.device_mut().refuse_memory = true;
    assert_eq!(channel.share(MemoryId(3)), Err(ChannelError::NotShared));
    assert!(channel.host().memory(GpadlId(1)).is_none());

    // Once the device takes it, it gets the ID the refusal did not use.
    channel.device_mut().refuse_memory = false;
    assert_eq!(channel.share(MemoryId(3)), Ok(GpadlId(1)));
}

#[test]
fn a_device_that_says_nothing_of_shared_memory_takes_it() {
    let mut channel = TestChannel::new(4096, 4096, Mute).unwrap();
    channel.add_memory(MemoryId(1), Memory::new(3));
    let gpadl = channel.share(MemoryId(1)).unwrap();
    assert_eq!(channel.host().memory(gpadl).unwrap().len(), 3 * 4096);
}

#[test]
#[should_panic]
fn reading_past_the_end_of_memory_panics_as_a_slice_does() {
    let memory = Memory::new(1);
    let mut bytes = [0; 2];
    memory.read(4095, &mut bytes);
}

#[test]
fn a_rescinded_channel_refuses_its_driver() {
    let mut channel = channel();
    channel.add_memory(MemoryId(1), Memory::new(1));
    channel.host().rescind();

    assert_eq!(channel.signal_host(), Err(ChannelError::Closed));
    assert_eq!(channel.wait(Some(0)), Err(ChannelError::Closed));
    assert_eq!(channel.wait(None), Err(ChannelError::Closed));
    assert_eq!(channel.share(MemoryId(1)), Err(ChannelError::Closed));
    assert_eq!(channel.host_signals(), 0);
    assert!(channel.device().seen.is_empty());
}

#[test]
fn a_device_that_leaves_while_the_driver_waits_ends_the_wait() {
    let mut channel = channel();
    channel.device_mut().leave_when_idle = true;
    assert_eq!(channel.wait(None), Err(ChannelError::Closed));
    assert_eq!(channel.signal_host(), Err(ChannelError::Closed));
}

#[test]
fn rings_are_whole_pages() {
    for (outgoing, incoming) in [(100, 4096), (4096, 100), (4096, 4097)] {
        assert!(matches!(
            TestChannel::new(outgoing, incoming, Mute),
            Err(vmbus_ring::Error::InvalidRingMemory)
        ));
    }
    assert!(TestChannel::new(8192, 4096, Mute).is_ok());
}

#[test]
fn a_channel_error_says_what_it_is() {
    assert_eq!(ChannelError::Closed.to_string(), "the channel is closed");
    assert_eq!(
        ChannelError::NotShared.to_string(),
        "the memory could not be shared with the host"
    );
    let _: &dyn core::error::Error = &ChannelError::Closed;
}
