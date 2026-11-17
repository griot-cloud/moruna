//! Random host packets from a fixed seed: whatever the host sends, the card neither
//! panics nor stops working.

use std::{cell::RefCell, rc::Rc};

use moruna_netvsc::NetvscError;
use netvsp_protocol::{
    protocol::{self, Status},
    rndisprot,
};
use vmbus_ring::TransferPageRange;
use zerocopy::IntoBytes;

use crate::fake::{
    Card, Fate, Got, Host, PAGE, RECEIVE_SET, ROOMY, Random, SMALL, drain, poll, rndis_frame,
    rndis_message, start_with, started,
};

/// Bytes of a receive section, and how many ROOMY has.
const SECTION: usize = 1806;
const SECTIONS: usize = 145;

/// A started card whose host judges nothing: it is the host that misbehaves here.
fn pair() -> (Card, Rc<RefCell<Host>>) {
    let (card, host) = started(ROOMY);
    host.borrow_mut().lenient = true;
    assert_eq!(host.borrow().section(), SECTION);
    assert_eq!(host.borrow().free_sections() + 16, SECTIONS);
    (card, host)
}

/// An RNDIS message for a receive section: a frame, an indication, a completion, or
/// noise, each sometimes well formed.
fn section(random: &mut Random) -> Vec<u8> {
    match random.below(100) {
        // A frame of a length a card may see.
        0..=29 => {
            let len = 14 + random.below(1501) as usize;
            rndis_frame(&random.bytes(len), SECTION)
        }
        // A frame whose fields point anywhere.
        30..=44 => {
            let mut message = rndis_frame(&random.bytes(64), SECTION);
            for field in [4, 8, 12] {
                if random.one_in(2) {
                    let value = match random.below(3) {
                        0 => random.below(2000) as u32,
                        1 => random.next() as u32,
                        _ => u32::MAX - random.below(16) as u32,
                    };
                    message[field..field + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
            message
        }
        // An indication: the link up, the link down, or something else.
        45..=64 => {
            let status = match random.below(4) {
                0 => rndisprot::STATUS_MEDIA_CONNECT,
                1 => rndisprot::STATUS_MEDIA_DISCONNECT,
                2 => rndisprot::STATUS_LINK_SPEED_CHANGE,
                _ => random.next() as u32,
            };
            let mut message = rndis_message(
                rndisprot::MESSAGE_TYPE_INDICATE_STATUS_MSG,
                &rndisprot::IndicateStatus {
                    status,
                    status_buffer_length: random.next() as u32,
                    status_buffer_offset: random.next() as u32,
                },
                &[],
            );
            if random.one_in(8) {
                message.truncate(8 + random.below(12) as usize);
            }
            message
        }
        // A completion of a request the card never made.
        65..=74 => {
            let kind = [
                rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT,
                rndisprot::MESSAGE_TYPE_QUERY_CMPLT,
                rndisprot::MESSAGE_TYPE_SET_CMPLT,
            ][random.below(3) as usize];
            let mut message = rndis_message(kind, &[random.next(); 4], &[]);
            if random.one_in(2) {
                let len = random.next() as u32;
                message[4..8].copy_from_slice(&len.to_le_bytes());
            }
            message
        }
        // Any message type, any length.
        75..=84 => {
            let header = rndisprot::MessageHeader {
                message_type: match random.below(3) {
                    0 => random.below(10) as u32,
                    1 => 0x8000_0000 | random.below(10) as u32,
                    _ => random.next() as u32,
                },
                message_length: match random.below(3) {
                    0 => random.below(64) as u32,
                    1 => random.below(4000) as u32,
                    _ => random.next() as u32,
                },
            };
            let len = random.below(200) as usize;
            [header.as_bytes(), &random.bytes(len)].concat()
        }
        // Noise.
        _ => {
            let len = random.below(SECTION as u64 + 1) as usize;
            random.bytes(len)
        }
    }
}

/// One random packet from the host; `false` when it left the ring itself unusable.
fn packet(random: &mut Random, host: &mut Host) -> bool {
    match random.below(100) {
        // A completion: of a section that might be in flight, or of anything.
        0..=14 => {
            let transaction = match random.below(3) {
                0 => random.below(12),
                1 => random.below(1 << 33),
                _ => random.next(),
            };
            let payload = match random.below(3) {
                0 => host.nvsp(
                    protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                    &protocol::Message1SendRndisPacketComplete {
                        status: Status(random.below(8) as u32),
                    },
                ),
                1 => host.nvsp(random.below(140) as u32, &random.next()),
                _ => {
                    let len = random.below(64) as usize;
                    random.bytes(len)
                }
            };
            host.raw_completion(transaction, payload);
        }
        // A message of its own: of a virtual function, or anything.
        15..=29 => {
            let transaction = random.one_in(2).then(|| random.next());
            let payload = match random.below(4) {
                0 => host.nvsp(
                    protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
                    &protocol::Message4SendVfAssociation {
                        vf_allocated: random.below(2) as u32,
                        serial_number: random.next() as u32,
                    },
                ),
                1 => {
                    let message = protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION;
                    let len = random.below(16) as usize;
                    [message.as_bytes(), &random.bytes(len)].concat()
                }
                2 => host.nvsp(random.below(140) as u32, &[random.next(); 3]),
                _ => {
                    let len = random.below(64) as usize;
                    random.bytes(len)
                }
            };
            host.raw_message(transaction, payload);
        }
        // A receive: mostly of whole sections of the receive buffer, each holding an
        // RNDIS message that is sometimes well formed.
        30..=94 => {
            let count = match random.below(20) {
                0 => 0,
                1 => 376 + random.below(8) as usize,
                _ => 1 + random.below(6) as usize,
            };
            let mut ranges = Vec::new();
            for _ in 0..count.min(8) {
                let section_at = random.below(SECTIONS as u64) as usize * SECTION;
                host.scribble(section_at, &section(random));
                let range = match random.below(10) {
                    0 => TransferPageRange {
                        byte_count: random.next() as u32,
                        byte_offset: random.next() as u32,
                    },
                    1 => TransferPageRange {
                        byte_count: random.below(2 * SECTION as u64) as u32,
                        byte_offset: random.below(64 * PAGE as u64 + 64) as u32,
                    },
                    2 => TransferPageRange {
                        byte_count: random.below(64) as u32,
                        byte_offset: section_at as u32,
                    },
                    _ => TransferPageRange {
                        byte_count: SECTION as u32,
                        byte_offset: section_at as u32,
                    },
                };
                ranges.push(range);
            }
            // Past eight, the ranges repeat: what matters is how many there are.
            while !ranges.is_empty() && ranges.len() < count {
                ranges.push(ranges[0]);
            }
            let set = if random.one_in(10) {
                random.next() as u16
            } else {
                RECEIVE_SET
            };
            let payload = match random.below(10) {
                0 => {
                    let len = random.below(64) as usize;
                    random.bytes(len)
                }
                1 => host.nvsp(random.below(140) as u32, &[random.next(); 3]),
                _ => host.rndis_packet(random.below(2) as u32),
            };
            host.raw_receive(random.next(), set, payload, ranges);
        }
        // A packet naming guest pages.
        95..=97 => {
            let len = random.below(64) as usize;
            let payload = random.bytes(len);
            host.raw_direct(random.next(), payload);
        }
        // A descriptor of any type, with any flags, its payload anywhere.
        98 => {
            let words = 1 + random.below(8) as usize;
            let rest = random.bytes(words * 8);
            let packet_type = [6, 7, 9, 0xb, random.next() as u16][random.below(5) as usize];
            host.raw_descriptor(
                packet_type,
                random.below(8) as u16,
                random.below(4) as u16,
                random.next(),
                &rest,
            );
        }
        // A write pointer that is no ring's.
        _ => {
            host.corrupt_ring();
            return false;
        }
    }
    true
}

/// The card carries a frame each way.
fn alive(card: &mut Card, host: &Rc<RefCell<Host>>, random: &mut Random) {
    let (out, back) = (60 + random.below(1455), 60 + random.below(1455));
    let (out, back) = (random.frame(out as usize), random.frame(back as usize));
    assert!(card.send(&out).unwrap());
    host.borrow_mut().inject(&back);
    assert_eq!(drain(card), [Got::Frame(back)]);
    assert_eq!(host.borrow().frames.last(), Some(&out));
}

#[test]
fn ten_thousand_random_host_packets_leave_the_card_working() {
    let mut random = Random(0x111a_2026_1117);
    let (mut card, mut host) = pair();
    let (mut frames, mut links, mut refused, mut rings) = (0, 0, 0, 0);
    for n in 0..10_000 {
        let usable = packet(&mut random, &mut host.borrow_mut());
        let mut ring_failed = false;
        loop {
            match poll(&mut card) {
                Ok(None) => break,
                Ok(Some(Got::Frame(frame))) => {
                    assert!((14..=1514).contains(&frame.len()), "{} bytes", frame.len());
                    frames += 1;
                }
                Ok(Some(Got::Link(up))) => {
                    assert_eq!(card.link_up(), up);
                    links += 1;
                }
                Err(NetvscError::Malformed(_) | NetvscError::NotSent(_)) => refused += 1,
                Err(NetvscError::Ring(_)) => {
                    ring_failed = true;
                    break;
                }
                Err(e) => panic!("packet {n}: {e}"),
            }
        }
        if ring_failed {
            // The ring is gone for good, as it is on every later poll. A driver ends
            // here; the run goes on with a new card.
            assert!(matches!(poll(&mut card), Err(NetvscError::Ring(_))));
            rings += 1;
            (card, host) = pair();
        } else {
            assert!(usable, "packet {n}: a corrupt ring went unnoticed");
            // Every hundredth packet, and the last: the card still carries frames.
            if n % 100 == 99 {
                alive(&mut card, &host, &mut random);
            }
        }
    }
    alive(&mut card, &host, &mut random);
    // The run reached every part: frames and link changes got through, most of the
    // rest was refused, and the ring broke now and then.
    // With this seed: 3,589 frames, 545 link changes, 14,180 refusals, 165 broken rings.
    assert!(frames > 3000, "{frames} frames");
    assert!(links > 400, "{links} link changes");
    assert!(refused > 10_000, "{refused} refusals");
    assert!(rings > 100, "{rings} broken rings");
}

#[test]
fn two_thousand_starts_against_random_answers_end_without_a_panic() {
    let mut random = Random(0x111a_0002);
    let (mut started, mut failed) = (0, 0);
    for n in 0..2000u64 {
        // One of the host's answers, chosen by how many came before it, is rewritten:
        // some bytes changed, or cut short, or grown.
        let target = random.below(14);
        let mut noise = Random(random.next());
        let (card, _host) = start_with(SMALL, |host| {
            let mut seen = 0;
            host.mangle = Some(Box::new(move |_, _, bytes| {
                seen += 1;
                if seen - 1 != target {
                    return Fate::Send;
                }
                match noise.below(4) {
                    0 => bytes.truncate(noise.below(bytes.len() as u64 + 1) as usize),
                    1 => {
                        let more = noise.below(64) as usize;
                        bytes.extend(noise.bytes(more));
                    }
                    _ => {}
                }
                for _ in 0..1 + noise.below(4) {
                    if !bytes.is_empty() {
                        let at = noise.below(bytes.len() as u64) as usize;
                        bytes[at] = match noise.below(3) {
                            0 => 0,
                            1 => 0xff,
                            _ => noise.next() as u8,
                        };
                    }
                }
                if noise.one_in(16) {
                    Fate::Drop
                } else {
                    Fate::Send
                }
            }));
        });
        match card {
            Ok(mut card) => {
                // Whatever the host changed, the card that started has an MTU its
                // receive sections and its send sections hold, and takes a frame, or
                // refuses it as longer than the MTU.
                assert!((1..=1748).contains(&card.mtu()), "start {n}");
                match card.send(&[0; 60]) {
                    Ok(true) | Err(NetvscError::Length(60)) => {}
                    other => panic!("start {n}: {other:?}"),
                }
                started += 1;
            }
            Err(_) => failed += 1,
        }
    }
    // With this seed: 1,076 starts and 924 failures.
    assert!(started > 800, "{started} starts");
    assert!(failed > 800, "{failed} failures");
}
