//! 10,000 random messages from the host, from a fixed seed, with the client's own
//! requests mixed in so that the messages find it in every state. Nothing here may
//! panic, and whatever the host sends, what the client says of itself stays true.

use std::collections::VecDeque;

use hvdef::HvMessageType;
use moruna_vmbus::Busy;
use moruna_vmbus::ChannelId;
use moruna_vmbus::Client;
use moruna_vmbus::ClientState;
use moruna_vmbus::Event;
use moruna_vmbus::GpadlId;
use moruna_vmbus::Hv;
use moruna_vmbus::HvMessage;
use moruna_vmbus::MAX_CHANNEL_ID;
use moruna_vmbus::MemoryId;
use moruna_vmbus::RingPair;
use vmbus_core::OutgoingMessage;
use vmbus_core::protocol;

use crate::host::NIC;
use crate::host::instance;

/// xorshift64*: small, fast, and the same on every machine.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// A host with no rules: it takes whatever is posted, when it is not busy, and its
/// messages are whatever the test queued.
struct Chaos {
    random: Random,
    inbox: VecDeque<HvMessage>,
    posted: usize,
    refused: usize,
}

impl Hv for Chaos {
    fn post_message(&mut self, _: u32, message_type: u32, payload: &[u8]) -> Result<(), Busy> {
        // Whatever the host does, what the client posts is a VMBus message that fits.
        assert_eq!(message_type, 1);
        assert!((8..=240).contains(&payload.len()));
        if self.random.one_in(6) {
            self.refused += 1;
            return Err(Busy);
        }
        self.posted += 1;
        Ok(())
    }

    fn signal_event(&mut self, _: u32, flag: u16) {
        assert_eq!(flag, 0);
    }

    fn take_message(&mut self) -> Option<HvMessage> {
        self.inbox.pop_front()
    }

    fn test_and_clear_event(&mut self, flag: u16) -> bool {
        assert!((1..=MAX_CHANNEL_ID).contains(&u32::from(flag)));
        self.random.one_in(2)
    }

    fn page_numbers(&mut self, memory: MemoryId) -> Option<Vec<u64>> {
        // Memory 0 is unknown; the others are as many pages as their ID.
        (memory.0 != 0).then(|| (0..memory.0).map(|page| 0x1000 + page).collect())
    }
}

/// What the test knows of the GPADLs the client has with the host, so that the host's
/// messages can be about them: a host that never answered for one would leave its
/// channel waiting for the rest of the run.
#[derive(Default)]
struct Gpadls {
    /// How many the client has made. Each open and each GPADL it takes is one.
    made: u32,
    /// Those the client has not yet reported gone: channel, GPADL, and whether it is a
    /// channel's rings.
    live: Vec<(ChannelId, GpadlId, bool)>,
}

impl Gpadls {
    fn add(&mut self, channel: ChannelId, rings: bool) -> GpadlId {
        self.made += 1;
        self.live.push((channel, GpadlId(self.made), rings));
        GpadlId(self.made)
    }

    /// Any live one, or failing that any small channel and GPADL.
    fn pick(&self, random: &mut Random) -> (ChannelId, GpadlId) {
        if self.live.is_empty() {
            return (
                ChannelId(random.below(4) as u32),
                GpadlId(random.below(u64::from(self.made) + 4) as u32),
            );
        }
        let (channel, gpadl, _) = self.live[random.below(self.live.len() as u64) as usize];
        (channel, gpadl)
    }

    fn forget(&mut self, event: Event) {
        match event {
            Event::GpadlFailed { gpadl, .. } | Event::GpadlTorndown { gpadl, .. } => {
                self.live.retain(|(_, live, _)| *live != gpadl);
            }
            Event::OpenFailed { channel, .. } | Event::Closed { channel } => {
                self.live
                    .retain(|(live, _, rings)| !(*rings && *live == channel));
            }
            Event::Unloaded => self.live.clear(),
            _ => {}
        }
    }
}

/// A message of the protocol that the client could be waiting for: most often about a
/// channel and a GPADL it has, and leaning towards what its state is waiting for, so
/// that the run gets past contact.
fn plausible(random: &mut Random, gpadls: &Gpadls, state: ClientState) -> OutgoingMessage {
    let (channel, gpadl) = if random.one_in(4) {
        (
            ChannelId(random.below(4) as u32),
            GpadlId(random.below(u64::from(gpadls.made) + 4) as u32),
        )
    } else {
        gpadls.pick(random)
    };
    let status = if random.one_in(6) { -1 } else { 0 };
    let kind = match state {
        ClientState::Connecting if !random.one_in(3) => 0,
        ClientState::RequestingOffers if !random.one_in(3) => 2 + 2 * random.below(2),
        ClientState::Disconnecting if random.one_in(2) => 11,
        _ => random.below(12),
    };
    match kind {
        0 | 1 => {
            let response = protocol::VersionResponse {
                version_supported: u8::from(!random.one_in(3)),
                connection_state: protocol::ConnectionState(u8::from(random.one_in(8))),
                padding: 0,
                selected_version_or_connection_id: random.below(3) as u32,
            };
            if random.one_in(2) {
                OutgoingMessage::new(&response)
            } else {
                OutgoingMessage::new(&protocol::VersionResponse2 {
                    version_response: response,
                    supported_features: random.next() as u32,
                })
            }
        }
        2 | 3 => {
            let id = match random.below(10) {
                0 => random.next() as u32,
                _ => random.below(4) as u32,
            };
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: NIC,
                instance_id: instance(id),
                rsvd: [0; 4],
                flags: protocol::OfferFlags::new(),
                mmio_megabytes: 0,
                user_defined: protocol::UserDefinedData::default(),
                subchannel_index: 0,
                mmio_megabytes_optional: 0,
                channel_id: ChannelId(id),
                monitor_id: 0xff,
                monitor_allocated: 0,
                is_dedicated: 1,
                connection_id: 0x2000 | (id & 0xfff),
            })
        }
        4 => OutgoingMessage::new(&protocol::AllOffersDelivered {}),
        5 if random.one_in(3) => OutgoingMessage::new(&protocol::RescindChannelOffer {
            channel_id: channel,
        }),
        5..=7 => OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: channel,
            gpadl_id: gpadl,
            status,
        }),
        8 => OutgoingMessage::new(&protocol::GpadlTorndown { gpadl_id: gpadl }),
        9 | 10 => OutgoingMessage::new(&protocol::OpenResult {
            channel_id: channel,
            open_id: u32::from(random.one_in(10)),
            status: status as u32,
        }),
        _ => OutgoingMessage::new(&protocol::UnloadComplete {}),
    }
}

/// One message from the host: plausible, a known type with random fields, or noise.
fn message(random: &mut Random, gpadls: &Gpadls, state: ClientState) -> HvMessage {
    match random.below(10) {
        0..=5 => HvMessage::new(HvMessageType(1), 0, plausible(random, gpadls, state).data()),
        6 | 7 => {
            // Any of the protocol's types, and a few that are not, over random fields of
            // a random length.
            let mut payload = (random.below(34) as u32).to_le_bytes().to_vec();
            payload.extend(random.bytes(4));
            let fields = random.below(233) as usize;
            payload.extend(random.bytes(fields));
            HvMessage::new(HvMessageType(1), 0, &payload)
        }
        _ => {
            // A slot of noise: any type, any length the header's byte can claim.
            let len = random.below(241) as usize;
            let mut slot = HvMessage::new(HvMessageType(1), random.next(), &random.bytes(len));
            if random.one_in(2) {
                slot.header.typ = HvMessageType(random.next() as u32);
            }
            if random.one_in(2) {
                slot.header.len = random.next() as u8;
            }
            slot
        }
    }
}

/// One request of the client's, with small numbers. Refusals are expected and ignored:
/// the point is the ones that are taken.
fn request(random: &mut Random, client: &mut Client, host: &mut Chaos, gpadls: &mut Gpadls) {
    let channel = ChannelId(random.below(4) as u32);
    let memory = MemoryId(random.below(40));
    match random.below(12) {
        0 | 1 => {
            let _ = client.connect();
        }
        2..=4 => {
            let ring = RingPair {
                memory,
                outgoing_pages: random.below(20) as u32,
            };
            if client.open(host, channel, ring).is_ok() {
                gpadls.add(channel, true);
            }
        }
        5 => {
            let _ = client.close(channel);
        }
        6 | 7 => {
            if let Ok(gpadl) = client.create_gpadl(host, channel, memory) {
                // A GPADL's ID is never used again: they count up from 1.
                assert_eq!(gpadl, gpadls.add(channel, false));
            }
        }
        8 => {
            let (own, gpadl) = gpadls.pick(random);
            let _ = client.teardown_gpadl(if random.one_in(4) { channel } else { own }, gpadl);
        }
        9 => {
            let _ = client.signal_host(host, channel);
        }
        10 => {
            let _ = client.take_signals(host);
        }
        _ => {
            if random.one_in(100) {
                let _ = client.unload();
            }
        }
    }
}

#[test]
fn ten_thousand_random_host_messages_never_panic_the_client() {
    let mut host = Chaos {
        random: Random(0x6d6f_7275_6e61_2031),
        inbox: VecDeque::new(),
        posted: 0,
        refused: 0,
    };
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut client = Client::new();
    let mut gpadls = Gpadls::default();

    let (mut taken, mut refused) = (0u32, 0u32);
    let mut seen = [0u32; 11];
    for _ in 0..10_000 {
        host.inbox
            .push_back(message(&mut random, &gpadls, client.state()));
        request(&mut random, &mut client, &mut host, &mut gpadls);
        // The message, and whatever it leads to: the client's own steps make no more
        // host messages here, so this ends.
        loop {
            match client.poll(&mut host) {
                Ok(Some(event)) => {
                    taken += 1;
                    seen[match event {
                        Event::Offer(_) => 0,
                        Event::Connected { .. } => 1,
                        Event::ConnectFailed(_) => 2,
                        Event::GpadlCreated { .. } => 3,
                        Event::GpadlFailed { .. } => 4,
                        Event::GpadlTorndown { .. } => 5,
                        Event::Opened { .. } => 6,
                        Event::OpenFailed { .. } => 7,
                        Event::Closed { .. } => 8,
                        Event::Rescinded { .. } => 9,
                        Event::Unloaded => 10,
                    }] += 1;
                    gpadls.forget(event);
                }
                Ok(None) => break,
                Err(_) => refused += 1,
            }
        }

        // What the client says of itself holds, whatever came.
        let offers: Vec<_> = client.offers().collect();
        assert!(offers.len() <= MAX_CHANNEL_ID as usize);
        for offer in &offers {
            assert!((1..=MAX_CHANNEL_ID).contains(&offer.channel.0));
            assert!(client.offer(offer.channel).is_some());
        }
        for id in 0..4 {
            if client.is_open(ChannelId(id)) {
                // Open until the host confirms an unload, if one is asked.
                assert!(matches!(
                    client.state(),
                    ClientState::Connected | ClientState::Disconnecting
                ));
                assert!(client.offer(ChannelId(id)).is_some());
            }
        }
        match client.state() {
            ClientState::Disconnected | ClientState::Connecting => {
                assert_eq!(client.version(), None);
            }
            _ => assert!(client.version().is_some()),
        }
        if client.state() == ClientState::Disconnected {
            assert_eq!(offers.len(), 0);
        }
    }

    // The run went everywhere. With this seed 9,325 messages are refused and 600 events
    // are taken, every kind among them (22 channels opened, 44 closed, 52 rescinded);
    // the client makes 318 GPADLs and posts 792 messages to a host that is busy for
    // 137 more. The floors below leave room for a change in the client's steps, not
    // for a run that stops short of any state.
    assert!(refused > 8_000, "{refused} refused");
    assert!(taken > 400, "{taken} taken");
    assert!(seen.iter().all(|count| *count > 0), "{seen:?}");
    assert!(gpadls.made > 200, "{} GPADLs", gpadls.made);
    assert!(host.posted > 500 && host.refused > 80);
}
