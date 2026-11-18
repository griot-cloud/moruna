//! Frames out and in: a thousand each way, the send buffer and the ring to the host
//! running out and recovering, a virtual function offered and ignored, the link going
//! down and coming up.

use moruna_netvsc::NetvscError;
use moruna_vmbus::Wait;
use netvsp_protocol::{
    protocol::{self, Status},
    rndisprot,
};

use crate::fake::{
    Card, Got, PAGE, ROOMY, Random, SMALL, Settings, assert_clean, down, drain, poll, start_on,
    start_with, started, up,
};

const SWITCH_DATA_PATH: u32 = protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH;

/// The length of the `i`th of a thousand frames: from 60 bytes, the shortest frame on
/// a wire, up to the MTU's 1,500 bytes and the header that goes with them.
fn length(i: usize) -> usize {
    60 + i * (1514 - 60) / 999
}

#[test]
fn a_thousand_frames_each_way_arrive_with_their_bytes_intact() {
    let (mut card, host) = started(ROOMY);
    let mut random = Random(0x111a);
    assert_eq!((length(0), length(999)), (60, 1514));
    assert_eq!(card.mtu() as usize + 14, length(999));
    // The frames out are as long as their number says; the frames in go the other way
    // through the lengths, so both directions carry every length.
    let out: Vec<Vec<u8>> = (0..1000).map(|i| random.frame(length(i))).collect();
    let back: Vec<Vec<u8>> = (0..1000).map(|i| random.frame(length(999 - i))).collect();
    let mut arrived = Vec::new();
    let (mut sent, mut delivered) = (0, 0);
    let mut round = 0;
    while sent < out.len() || delivered < back.len() {
        round += 1;
        // Out: as many as the send buffer takes, up to a few a round.
        for _ in 0..1 + round % 7 {
            if sent < out.len() && card.send(&out[sent]).unwrap() {
                sent += 1;
            }
        }
        // In: one receive of one to eight frames, a section each.
        let batch = (1 + round % 8).min(back.len() - delivered);
        if batch > 0 {
            let frames: Vec<&[u8]> = back[delivered..delivered + batch]
                .iter()
                .map(Vec::as_slice)
                .collect();
            host.borrow_mut().deliver(&frames);
            delivered += batch;
        }
        for got in drain(&mut card) {
            match got {
                Got::Frame(frame) => arrived.push(frame),
                Got::Link(up) => panic!("the link changed to {up}"),
            }
        }
    }
    assert_eq!(drain(&mut card), []);
    assert_clean(&host);
    let host = host.borrow();
    assert_eq!(host.frames.len(), 1000);
    assert!(host.frames == out, "a frame out changed on the way");
    assert_eq!(arrived.len(), 1000);
    assert!(arrived == back, "a frame in changed on the way");
    // Every section of the host's is its own again, each receive completed as a
    // success: the four of initialisation, then one for each batch.
    assert_eq!(host.free_sections(), 129);
    assert!(host.received.iter().all(|s| *s == Status::SUCCESS));
    assert!(host.received.len() > 4 + 1000 / 8);
}

#[test]
fn the_send_buffer_runs_out_and_recovers() {
    // Sixteen kilobytes of send buffer in sections of 6,144 bytes: two sections.
    let (mut card, host) = started(SMALL);
    let mut random = Random(2);
    let frames: Vec<Vec<u8>> = (0..40).map(|i| random.frame(60 + 36 * i)).collect();
    let mut next = 0;
    for cycle in 0..10 {
        // The host takes frames and keeps its completions: after two, no section is
        // free, and the frame waits.
        host.borrow_mut().hold = true;
        assert!(card.send(&frames[next]).unwrap(), "cycle {cycle}");
        assert!(card.send(&frames[next + 1]).unwrap(), "cycle {cycle}");
        for _ in 0..3 {
            assert!(!card.send(&frames[next + 2]).unwrap(), "cycle {cycle}");
            assert_eq!(drain(&mut card), []);
        }
        assert_eq!(host.borrow().frames.len(), next + 2);
        // The completions come, and with them the sections.
        host.borrow_mut().release();
        assert_eq!(drain(&mut card), []);
        assert!(card.send(&frames[next + 2]).unwrap(), "cycle {cycle}");
        assert!(card.send(&frames[next + 3]).unwrap(), "cycle {cycle}");
        assert_eq!(drain(&mut card), []);
        next += 4;
    }
    assert_clean(&host);
    assert!(
        host.borrow().frames == frames,
        "a frame changed or was lost"
    );
}

#[test]
fn the_ring_to_the_host_fills_and_recovers() {
    // The ring to the host is one page, and the send buffer has 128 sections of 2,048
    // bytes, so it is the ring that runs out when the host stops reading.
    let settings = Settings {
        receive_buffer: 8 * PAGE,
        send_buffer: 64 * PAGE,
    };
    let (card, host) = start_on(PAGE, 16 * PAGE, settings, |host| host.send_section = 2048);
    let mut card = up(card);
    // Take the signal the host left while the card started.
    assert_eq!(card.wait(), Ok(Wait::Signalled));
    let mut random = Random(3);
    let frames: Vec<Vec<u8>> = (0..70).map(|i| random.frame(100 + i)).collect();
    host.borrow_mut().paused = true;
    let mut sent = 0;
    while card.send(&frames[sent]).unwrap() {
        sent += 1;
    }
    // Each frame's packet takes 64 bytes of the ring's 4,088.
    assert_eq!(sent, 63);
    assert!(!card.send(&frames[sent]).unwrap());
    assert!(host.borrow().frames.is_empty());
    assert_eq!(card.wait(), Ok(Wait::TimedOut));
    // The host reads again. It was asked to signal once the ring has room, and does.
    host.borrow_mut().paused = false;
    card.run_host();
    assert_eq!(card.wait(), Ok(Wait::Signalled));
    assert_eq!(drain(&mut card), []);
    for frame in &frames[sent..] {
        assert!(card.send(frame).unwrap());
        assert_eq!(drain(&mut card), []);
    }
    assert_clean(&host);
    assert!(
        host.borrow().frames == frames,
        "a frame changed or was lost"
    );
}

#[test]
fn arm_says_when_there_is_work_and_leaves_the_hosts_signal_on() {
    // The ring to the host is one page, so it is the ring that runs out.
    let settings = Settings {
        receive_buffer: 8 * PAGE,
        send_buffer: 64 * PAGE,
    };
    let (card, host) = start_on(PAGE, 16 * PAGE, settings, |host| host.send_section = 2048);
    let mut card = up(card);
    assert_eq!(card.wait(), Ok(Wait::Signalled));
    assert_eq!(drain(&mut card), []);
    // Nothing to read and nothing refused: nothing to do.
    assert!(!card.arm().unwrap());
    // A frame comes: there is work, and the host signalled as it delivered.
    let mut random = Random(0xa4);
    let frame = random.frame(200);
    host.borrow_mut().deliver(&[&frame]);
    assert!(card.arm().unwrap(), "the frame is in the ring");
    assert_eq!(drain(&mut card), [Got::Frame(frame)]);
    assert!(!card.arm().unwrap());
    assert_eq!(card.wait(), Ok(Wait::Signalled), "the host's signal, kept");
    // The host stops reading and keeps its completions: the ring to it fills, and a
    // frame is refused. Nothing to read and no room: nothing to do yet.
    let frames: Vec<Vec<u8>> = (0..70).map(|i| random.frame(100 + i)).collect();
    host.borrow_mut().paused = true;
    host.borrow_mut().hold = true;
    let mut sent = 0;
    while card.send(&frames[sent]).unwrap() {
        sent += 1;
    }
    assert!(!card.arm().unwrap());
    // The host reads again and answers nothing: the room alone is work.
    host.borrow_mut().paused = false;
    card.run_host();
    assert!(card.arm().unwrap(), "the ring to the host has room");
    for frame in &frames[sent..] {
        assert!(card.send(frame).unwrap());
    }
    assert!(!card.arm().unwrap(), "the room was used and nothing came");
    host.borrow_mut().release();
    assert_eq!(drain(&mut card), []);
    assert_clean(&host);
    assert!(
        host.borrow().frames == frames,
        "a frame changed or was lost"
    );
}

#[test]
fn the_ring_to_the_guest_fills_and_the_host_is_told_when_it_has_room() {
    // The ring to the guest is one page: 51 receives of one frame. The host has a
    // hundred to deliver, asks to be signalled when the ring has room, and sends the
    // rest as the card reads.
    let (card, host) = start_on(16 * PAGE, PAGE, ROOMY, |_| ());
    let mut card = up(card);
    let mut random = Random(6);
    let frames: Vec<Vec<u8>> = (0..100).map(|i| random.frame(60 + 14 * i)).collect();
    let signals = card.signals();
    for frame in &frames {
        host.borrow_mut().deliver(&[frame]);
    }
    let got = drain(&mut card);
    assert_eq!(got.len(), 100);
    assert!(got.into_iter().eq(frames.iter().cloned().map(Got::Frame)));
    assert!(card.signals() > signals);
    assert_eq!(host.borrow().free_sections(), 129);
    assert_clean(&host);
}

#[test]
fn completions_wait_for_room_on_the_ring_and_go_in_order() {
    // The host delivers seventy receives and reads nothing. The ring to it holds 63
    // completions; the rest are owed until it reads.
    let (card, host) = start_on(PAGE, 16 * PAGE, ROOMY, |_| ());
    let mut card = up(card);
    let before = host.borrow().received.len();
    host.borrow_mut().paused = true;
    let mut random = Random(4);
    let frames: Vec<Vec<u8>> = (0..70).map(|_| random.frame(200)).collect();
    for frame in &frames {
        host.borrow_mut().deliver(&[frame]);
    }
    let got = drain(&mut card);
    assert_eq!(got.len(), 70);
    assert!(got.into_iter().eq(frames.iter().cloned().map(Got::Frame)));
    assert_eq!(host.borrow().free_sections(), 129 - 70);
    // The host reads: 63 sections come back. The next poll writes the seven owed.
    host.borrow_mut().paused = false;
    card.run_host();
    assert_eq!(host.borrow().free_sections(), 129 - 7);
    assert_eq!(drain(&mut card), []);
    assert_eq!(host.borrow().free_sections(), 129);
    assert_eq!(host.borrow().received.len(), before + 70);
    assert_clean(&host);
    // A send also writes what is owed, before the frame.
    host.borrow_mut().paused = true;
    for frame in &frames {
        host.borrow_mut().deliver(&[frame]);
    }
    assert_eq!(drain(&mut card).len(), 70);
    host.borrow_mut().paused = false;
    card.run_host();
    assert!(card.send(&frames[0]).unwrap());
    assert_eq!(host.borrow().free_sections(), 129);
    assert_eq!(host.borrow().frames, [frames[0].clone()]);
    assert_clean(&host);
}

#[test]
fn a_frames_section_goes_back_only_after_the_frame_was_handed_over() {
    let (mut card, host) = started(ROOMY);
    let frames = [vec![1; 60], vec![2; 700], vec![3; 1514]];
    let all: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let before = host.borrow().received.len();
    host.borrow_mut().deliver(&all);
    // The three frames come one poll at a time, and the host still waits for all
    // three sections while the caller has the last frame.
    for frame in &frames {
        assert_eq!(poll(&mut card).unwrap(), Some(Got::Frame(frame.clone())));
        assert_eq!(host.borrow().free_sections(), 129 - 3);
        assert_eq!(host.borrow().received.len(), before);
    }
    // The poll after the last frame gives the sections back, with one completion.
    assert_eq!(poll(&mut card).unwrap(), None);
    assert_eq!(host.borrow().free_sections(), 129);
    assert_eq!(host.borrow().received.len(), before + 1);
    assert_clean(&host);
}

#[test]
fn a_virtual_function_offered_is_acknowledged_and_ignored_and_traffic_continues() {
    let (mut card, host) = started(ROOMY);
    let mut random = Random(5);
    // Ten frames each way, each checked as it arrives.
    let mut exchange = |card: &mut Card| {
        for _ in 0..10 {
            let out = random.frame(300);
            let back = random.frame(900);
            assert!(card.send(&out).unwrap());
            host.borrow_mut().inject(&back);
            assert_eq!(drain(card), [Got::Frame(back)]);
            assert_eq!(host.borrow().frames.last(), Some(&out));
        }
    };
    exchange(&mut card);
    // The host associates a virtual function, as Azure does with accelerated
    // networking: the card acknowledges it and reports nothing.
    host.borrow_mut().offer_vf(0x1234);
    assert_eq!(drain(&mut card), []);
    assert_eq!(host.borrow().vf_acknowledged, 1);
    exchange(&mut card);
    // The host takes it away again, without asking for an acknowledgement this time.
    let gone = host.borrow().nvsp(
        protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION,
        &protocol::Message4SendVfAssociation {
            vf_allocated: 0,
            serial_number: 0x1234,
        },
    );
    host.borrow_mut().raw_message(None, gone);
    assert_eq!(drain(&mut card), []);
    assert_eq!(host.borrow().vf_acknowledged, 1);
    exchange(&mut card);
    // In all of it the card never asked for the data path to move.
    assert!(!host.borrow().messages.contains(&SWITCH_DATA_PATH));
    assert_eq!(host.borrow().frames.len(), 30);
    assert_clean(&host);
}

#[test]
fn a_link_down_then_up_is_reported() {
    let (mut card, host) = started(ROOMY);
    assert!(card.link_up());
    host.borrow_mut().link(false);
    assert_eq!(poll(&mut card).unwrap(), Some(Got::Link(false)));
    assert!(!card.link_up());
    assert_eq!(poll(&mut card).unwrap(), None);
    host.borrow_mut().link(true);
    assert_eq!(poll(&mut card).unwrap(), Some(Got::Link(true)));
    assert!(card.link_up());
    assert_eq!(poll(&mut card).unwrap(), None);
    // Down and up again before the caller looks: both are reported, in order.
    host.borrow_mut().link(false);
    host.borrow_mut().link(true);
    assert_eq!(drain(&mut card), [Got::Link(false), Got::Link(true)]);
    assert!(card.link_up());
    // An indication of the state the link is already in is no change.
    host.borrow_mut().link(true);
    assert_eq!(drain(&mut card), []);
    // Nor is an indication of anything else.
    host.borrow_mut()
        .indicate(rndisprot::STATUS_LINK_SPEED_CHANGE);
    host.borrow_mut()
        .indicate(rndisprot::STATUS_TASK_OFFLOAD_CURRENT_CONFIG);
    assert_eq!(drain(&mut card), []);
    assert!(card.link_up());
    // Frames come between indications, in the order the host sent them.
    host.borrow_mut().link(false);
    host.borrow_mut().deliver(&[&[6; 80]]);
    host.borrow_mut().link(true);
    assert_eq!(
        drain(&mut card),
        [Got::Link(false), Got::Frame(vec![6; 80]), Got::Link(true)]
    );
    // Every indication's section went back to the host, and the frame's.
    assert_eq!(host.borrow().free_control_sections(), 16);
    assert_eq!(host.borrow().free_sections(), 129);
    assert_clean(&host);
}

#[test]
fn a_frame_no_card_sends_is_refused_before_it_is_sent() {
    let (mut card, host) = started(SMALL);
    for len in [0, 1, 13, 1515, 9000] {
        let e = card.send(&vec![0; len]).unwrap_err();
        assert!(matches!(e, NetvscError::Length(n) if n == len), "{e}");
        assert_eq!(e.to_string(), format!("a frame of {len} bytes"));
    }
    // The shortest and the longest frame go.
    assert!(card.send(&[0; 14]).unwrap());
    assert!(card.send(&[0; 1514]).unwrap());
    assert_eq!(host.borrow().frames, [vec![0; 14], vec![0; 1514]]);
    assert_clean(&host);
}

#[test]
fn a_host_whose_send_sections_cannot_hold_the_largest_frame_fails_the_start() {
    // The largest frame of a 1,500-byte MTU is 1,514 bytes, 1,558 with its RNDIS
    // fields. Sections of 1,557 bytes cannot hold it: the card does not start, and the
    // packet filter was never set, so the host delivers nothing.
    let (card, host) = start_with(SMALL, |host| host.send_section = 1557);
    let e = down(card);
    assert!(
        matches!(
            e,
            NetvscError::Unfit {
                message: 1558,
                section: 1557
            }
        ),
        "{e}"
    );
    assert_eq!(
        e.to_string(),
        "a frame whose message of 1558 bytes fits no send section of 1557 bytes"
    );
    assert_eq!(host.borrow().filter, 0);
    assert_clean(&host);
    // A smaller MTU fits smaller sections: 1,024 bytes hold the 980-byte frame of an
    // MTU of 966.
    let (card, host) = start_with(SMALL, |host| {
        host.send_section = 1024;
        host.mtu = Some(966);
    });
    let mut card = up(card);
    assert!(card.send(&[1; 980]).unwrap());
    assert!(matches!(
        card.send(&[1; 981]),
        Err(NetvscError::Length(981))
    ));
    assert_eq!(host.borrow().frames, [vec![1; 980]]);
    // Sections of 1,558 bytes hold the largest frame of a 1,500-byte MTU, exactly.
    let (card, host) = start_with(SMALL, |host| host.send_section = 1558);
    let mut card = up(card);
    assert!(card.send(&[2; 1514]).unwrap());
    assert_eq!(host.borrow().frames, [vec![2; 1514]]);
    assert_clean(&host);
}

#[test]
fn a_frame_the_host_could_not_send_is_reported_and_its_section_freed() {
    let (mut card, host) = started(SMALL);
    host.borrow_mut().send_status = Status::FAILURE;
    assert!(card.send(&[1; 60]).unwrap());
    let e = poll(&mut card).unwrap_err();
    assert!(matches!(e, NetvscError::NotSent(2)), "{e}");
    assert_eq!(e.to_string(), "the host could not send a frame (status 2)");
    assert_eq!(poll(&mut card).unwrap(), None);
    // Both sections are free: three more frames go, two at a time.
    host.borrow_mut().send_status = Status::SUCCESS;
    host.borrow_mut().hold = true;
    assert!(card.send(&[2; 60]).unwrap());
    assert!(card.send(&[3; 60]).unwrap());
    assert!(!card.send(&[4; 60]).unwrap());
    host.borrow_mut().release();
    assert_eq!(drain(&mut card), []);
    assert!(card.send(&[4; 60]).unwrap());
    assert_clean(&host);
}
