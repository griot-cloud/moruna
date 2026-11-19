//! NT-T1 to NT-T4 (SDD 29, k): the protocol's types on the wire, the frame checks,
//! the flow hash, and the receive hub.

use alloc::{vec, vec::Vec};

use moruna_svc::{Channel, Decoder, Encoder, Handle, Kind, Wire, frame};

use crate::{
    Answer, FRAME_MAX, FRAME_MIN, Frame, Hub, Info, Link, QUEUE_FRAMES, RECEIVE_BYTES,
    SEND_FRAMES_MAX, Stats, answer_frames, check, flow_hash, is_group, mac, net, octets,
    queue_frames, receives,
};

fn round_trip<T: Wire + Clone + PartialEq + core::fmt::Debug>(v: T) {
    let mut e = Encoder::new();
    v.clone().encode(&mut e).expect("encode");
    let (mine, theirs) = Channel::create().expect("channel");
    frame::send(&mine, Kind::Event, 1, 0, e).expect("send");
    let mut f = frame::recv(&theirs).expect("recv").expect("a frame");
    let mut d = f.decoder();
    assert_eq!(T::decode(&mut d), Ok(v));
    d.finish().expect("nothing trailing");
}

#[test]
fn nt_t1_the_types_travel() {
    round_trip(Frame(vec![1, 2, 3]));
    round_trip(Frame(Vec::new()));
    round_trip(vec![Frame(vec![9; 1514]), Frame(vec![7; 60])]);
    round_trip(Info {
        mac: 0x5254_0012_3456,
        mtu: 1500,
        max_frame: 1514,
        offloads: 0,
        queues: 2,
    });
    round_trip(Link {
        up: true,
        speed_mbps: 10_000,
        generation: 3,
    });
    round_trip(Stats {
        tx_frames: 1,
        tx_bytes: 2,
        rx_frames: 3,
        rx_bytes: 4,
        rx_dropped: 5,
        rx_errors: 6,
        tx_errors: 7,
    });
    // A frame whose length runs past the message is refused, not read past.
    let mut bytes = 1000u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[1, 2, 3]);
    let mut handles: [Option<Handle>; 0] = [];
    assert!(Frame::decode(&mut Decoder::new(&bytes, &mut handles)).is_err());
    assert_eq!(octets(0x5254_0012_3456), [0x52, 0x54, 0, 0x12, 0x34, 0x56]);
    assert_eq!(mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]), 0x5254_0012_3456);
    assert!(is_group(0xffff_ffff_ffff));
    assert!(is_group(0x0100_5e00_0001));
    assert!(!is_group(0x5254_0012_3456));
    assert_eq!(net::ID.name, "moruna.driver.net");
    assert_eq!((net::ID.major, net::ID.minor), (1, 1));
    assert_eq!(net::REQUESTS, &[1, 2, 3, 4, 5, 6, 7]);
}

fn eth(src: [u8; 6], len: usize) -> Frame {
    let mut f = vec![0u8; len];
    if len >= 12 {
        f[6..12].copy_from_slice(&src);
    }
    Frame(f)
}

#[test]
fn nt_t2_a_batch_a_card_must_not_send_is_refused() {
    let me = [0x52, 0x54, 0, 1, 2, 3];
    assert_eq!(check(&[eth(me, FRAME_MIN)], 1514), Ok(()));
    assert_eq!(check(&[eth(me, 1514)], 1514), Ok(()));
    assert_eq!(check(&[eth(me, 13)], 1514), Err(net::Error::TooShort));
    assert_eq!(check(&[Frame(Vec::new())], 1514), Err(net::Error::TooShort));
    assert_eq!(check(&[eth(me, 1515)], 1514), Err(net::Error::TooLong));
    assert_eq!(
        check(&[eth([0xff; 6], 60)], 1514),
        Err(net::Error::BadSource)
    );
    assert_eq!(
        check(&[eth([0x01, 0, 0x5e, 0, 0, 1], 60)], 1514),
        Err(net::Error::BadSource)
    );
    assert_eq!(check(&[], 1514), Err(net::Error::Empty));
    let many: Vec<Frame> = (0..=SEND_FRAMES_MAX).map(|_| eth(me, 60)).collect();
    assert_eq!(check(&many, 1514), Err(net::Error::TooMany));
    assert_eq!(check(&many[1..], 1514), Ok(()));
    assert_eq!(
        check(&[eth(me, 60), eth(me, 2)], 1514),
        Err(net::Error::TooShort)
    );
}

fn ipv4(proto: u8, src: [u8; 4], dst: [u8; 4], ports: (u16, u16), fragment: bool) -> Vec<u8> {
    let mut f = vec![0u8; 14];
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    if fragment {
        ip[6..8].copy_from_slice(&0x0010u16.to_be_bytes());
    }
    ip[9] = proto;
    ip[12..16].copy_from_slice(&src);
    ip[16..20].copy_from_slice(&dst);
    f.extend_from_slice(&ip);
    f.extend_from_slice(&ports.0.to_be_bytes());
    f.extend_from_slice(&ports.1.to_be_bytes());
    f.extend_from_slice(&[0; 16]);
    f
}

#[test]
fn nt_t3_a_flow_hashes_alike_and_other_frames_to_zero() {
    let a = ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], (40000, 443), false);
    let mut payload_differs = a.clone();
    payload_differs[40] = 0xee;
    assert_eq!(flow_hash(&a), flow_hash(&payload_differs));
    assert_ne!(
        flow_hash(&a),
        flow_hash(&ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], (40001, 443), false))
    );
    assert_ne!(
        flow_hash(&a),
        flow_hash(&ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], (40000, 443), false))
    );
    // A fragment has no ports in it: it hashes by its addresses, as every fragment of
    // its datagram does.
    let f1 = ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], (1, 2), true);
    let f2 = ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], (3, 4), true);
    assert_eq!(flow_hash(&f1), flow_hash(&f2));
    assert_eq!(flow_hash(&[0u8; 60]), 0);
    assert_eq!(flow_hash(&[]), 0);
    assert_eq!(flow_hash(&a[..20]), 0);
    let mut v6 = vec![0u8; 14 + 44];
    v6[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
    v6[14] = 0x60;
    v6[14 + 6] = 6;
    v6[14 + 8] = 0xfe;
    let h = flow_hash(&v6);
    v6[14 + 40] = 1;
    assert_ne!(h, flow_hash(&v6));
}

#[test]
fn nt_t4_the_hub_fans_frames_out_and_bounds_every_queue() {
    // Clients of 1.0: one receive waits at a time, as in 1.0.
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    assert_eq!(hub.receive(1, 8, || 100), Err(net::Error::NotSubscribed));
    hub.subscribe(1, 0);
    hub.subscribe(1, 0);
    hub.subscribe(2, 0);
    assert_eq!(hub.subscribers(), 2);
    // No frame: the receive waits; a second waits are refused.
    assert_eq!(hub.receive(1, 8, || 100), Ok(Answer::Later));
    assert_eq!(hub.receive(1, 8, || 101), Err(net::Error::InUse));
    hub.deliver(&[1; 60]);
    hub.deliver(&[2; 70]);
    let mut out = Vec::new();
    hub.answers(&mut out);
    assert_eq!(out, [(100, vec![Frame(vec![1; 60]), Frame(vec![2; 70])])]);
    // The other connection got its own copies.
    assert_eq!(
        hub.receive(2, 1, || 0),
        Ok(Answer::Now(vec![Frame(vec![1; 60])]))
    );
    assert_eq!(
        hub.receive(2, 0, || 0),
        Ok(Answer::Now(vec![Frame(vec![2; 70])]))
    );
    assert_eq!(hub.stats(2).rx_frames, 2);
    assert_eq!(hub.stats(2).rx_bytes, 130);
    // Three receives answered with four frames; the first of connection 2's left one queued
    // behind it (work item 6.3ab).
    assert_eq!((hub.answered, hub.answered_frames, hub.backlog), (3, 4, 1));
    // A full queue drops and counts; the other connection's does not.
    for _ in 0..QUEUE_FRAMES + 5 {
        hub.deliver(&[3; 60]);
    }
    assert_eq!(hub.stats(1).rx_dropped, 5);
    assert_eq!(hub.stats(9).rx_dropped, 0);
    // Both connections' queues were full: ten drops in all.
    assert_eq!(hub.rx_dropped, 10);
    // One answer never exceeds a message, however many frames are queued.
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    hub.subscribe(1, 0);
    for _ in 0..100 {
        hub.deliver(&[4; 1514]);
    }
    let Ok(Answer::Now(first)) = hub.receive(1, 1024, || 0) else {
        panic!("frames were queued");
    };
    let bytes: usize = first.iter().map(|f| 4 + f.0.len()).sum();
    assert!(bytes <= RECEIVE_BYTES && bytes + 4 + 1514 > RECEIVE_BYTES);
    // The largest frame the protocol carries goes, alone, in one reply.
    let mut hub: Hub<u32> = Hub::new(true, 0, FRAME_MAX as u32);
    hub.subscribe(1, 0);
    hub.deliver(&vec![5; FRAME_MAX]);
    hub.deliver(&vec![5; FRAME_MAX]);
    let Ok(Answer::Now(alone)) = hub.receive(1, 8, || 0) else {
        panic!("a frame was queued");
    };
    assert_eq!(alone.len(), 2);
    // A closed connection's queue, waiting receive and link waits go.
    let mut hub: Hub<u32> = Hub::new(false, 0, 1514);
    hub.subscribe(1, 1);
    assert_eq!(hub.receive(1, 8, || 7), Ok(Answer::Later));
    assert_eq!(hub.receive(1, 8, || 11), Ok(Answer::Later));
    assert_eq!(hub.link_changed(1, 1, || 8), Answer::Later);
    assert_eq!(hub.link_changed(2, 1, || 9), Answer::Later);
    hub.closed(1);
    hub.deliver(&[6; 60]);
    let mut out = Vec::new();
    hub.answers(&mut out);
    assert!(out.is_empty());
    // The link: a change answers the waiters with the new generation; no change
    // answers nobody; another generation is answered at once.
    let mut changes = Vec::new();
    hub.set_link(false, 0, &mut changes);
    assert!(changes.is_empty());
    hub.set_link(true, 1000, &mut changes);
    let up = Link {
        up: true,
        speed_mbps: 1000,
        generation: 2,
    };
    assert_eq!(changes, [(9, up)]);
    assert_eq!(hub.link(), up);
    assert_eq!(hub.link_changed(2, 1, || 10), Answer::Now(up));
    hub.tx_frames = 4;
    hub.tx_errors = 1;
    hub.rx_errors = 2;
    let s = hub.stats(2);
    assert_eq!((s.tx_frames, s.tx_errors, s.rx_errors), (4, 1, 2));
}

/// `n` frames of `len` bytes, each numbered in its first four bytes from `first`.
fn numbered(first: u32, n: u32, len: usize) -> Vec<Vec<u8>> {
    (first..first + n)
        .map(|k| {
            let mut f = vec![0u8; len];
            f[..4].copy_from_slice(&k.to_be_bytes());
            f
        })
        .collect()
}

#[test]
fn nt_t4_the_count_of_receives_is_a_full_queue_over_an_answer() {
    // MTU 1500: 1024 frames a queue, 43 of the largest an answer, 24 receives.
    assert_eq!(queue_frames(1514), QUEUE_FRAMES as u32);
    assert_eq!(answer_frames(256, 1514), 43);
    assert_eq!(answer_frames(1024, 1514), 43);
    assert_eq!(receives(256, 1514), 24);
    // A smaller batch takes more receives; a batch is clamped to 1 to QUEUE_FRAMES.
    assert_eq!(answer_frames(8, 1514), 8);
    assert_eq!(receives(8, 1514), 128);
    assert_eq!(answer_frames(0, 1514), 1);
    assert_eq!(receives(0, 1514), QUEUE_FRAMES as u32);
    // The largest frames: the queue's bytes bound it first (256), and an answer holds 3.
    assert_eq!(queue_frames(FRAME_MAX as u32), 256);
    assert_eq!(answer_frames(256, FRAME_MAX as u32), 3);
    assert_eq!(receives(256, FRAME_MAX as u32), 86);
    // Small frames: an answer takes the whole batch.
    assert_eq!(answer_frames(256, 60), 256);
    assert_eq!(receives(256, 60), 4);
    // No frame size divides by zero or answers with nothing.
    assert_eq!(queue_frames(0), QUEUE_FRAMES as u32);
    assert_eq!(queue_frames(u32::MAX), 1);
    assert_eq!(answer_frames(5, u32::MAX), 1);
}

#[test]
fn nt_t4_a_client_of_1_1_keeps_receives_waiting_until_they_take_a_full_queue() {
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    hub.subscribe(1, 1);
    hub.subscribe(2, 0);
    // A client of 1.1: 24 receives of 43 frames wait, the 25th is refused.
    for k in 0..24 {
        assert_eq!(hub.receive(1, 256, || k), Ok(Answer::Later), "receive {k}");
    }
    assert_eq!(hub.receive(1, 256, || 99), Err(net::Error::InUse));
    // A client of 1.0 beside it: one.
    assert_eq!(hub.receive(2, 256, || 200), Ok(Answer::Later));
    assert_eq!(hub.receive(2, 256, || 201), Err(net::Error::InUse));
    // A full queue of the largest frames: every frame goes out to the 1.1 client in the
    // order it came, in its receives in the order they came, and none stays queued; the
    // 1.0 client's one receive takes 43 and leaves the rest queued.
    let burst = numbered(0, QUEUE_FRAMES as u32, 1514);
    for f in &burst {
        hub.deliver(f);
    }
    let mut out = Vec::new();
    hub.answers(&mut out);
    let (ones, twos): (Vec<_>, Vec<_>) = out.into_iter().partition(|(p, _)| *p < 200);
    assert_eq!(
        ones.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
        (0..24).collect::<Vec<_>>()
    );
    let got: Vec<Vec<u8>> = ones.into_iter().flat_map(|(_, f)| f).map(|f| f.0).collect();
    assert!(got == burst, "the frames in order");
    assert_eq!(twos.len(), 1);
    assert_eq!(twos[0].1.len(), 43);
    assert_eq!(hub.rx_dropped, 0);
    // 25 answers; only the 1.0 client's left frames queued.
    assert_eq!((hub.answered, hub.answered_frames), (25, 1024 + 43));
    assert_eq!(hub.backlog, 1);

    // More frames than the receives waiting take: answered in order, and backlog once,
    // for frames still queued when every receive waiting has been answered.
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    hub.subscribe(1, 1);
    assert_eq!(hub.receive(1, 256, || 1), Ok(Answer::Later));
    assert_eq!(hub.receive(1, 256, || 2), Ok(Answer::Later));
    for f in numbered(0, 100, 1514) {
        hub.deliver(&f);
    }
    let mut out = Vec::new();
    hub.answers(&mut out);
    let sizes: Vec<(u32, usize)> = out.iter().map(|(p, f)| (*p, f.len())).collect();
    assert_eq!(sizes, [(1, 43), (2, 43)]);
    assert_eq!((hub.answered, hub.backlog), (2, 1));
    // The queue holds frames and nothing waits: a receive is answered at once, and is
    // backlog if it leaves frames queued.
    let Ok(Answer::Now(now)) = hub.receive(1, 10, || 3) else {
        panic!("frames were queued");
    };
    assert_eq!(now.len(), 10);
    assert_eq!((hub.answered, hub.backlog), (3, 2));
    let Ok(Answer::Now(rest)) = hub.receive(1, 256, || 4) else {
        panic!("frames were queued");
    };
    assert_eq!(rest.len(), 4);
    assert_eq!((hub.answered, hub.backlog), (4, 2));

    // A receive that comes while frames are queued for one waiting before it waits behind
    // it, and the frames go to the first.
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    hub.subscribe(1, 1);
    assert_eq!(hub.receive(1, 256, || 1), Ok(Answer::Later));
    hub.deliver(&[7; 60]);
    assert_eq!(hub.receive(1, 256, || 2), Ok(Answer::Later));
    let mut out = Vec::new();
    hub.answers(&mut out);
    assert_eq!(out, [(1, vec![Frame(vec![7; 60])])]);
    hub.deliver(&[8; 60]);
    let mut out = Vec::new();
    hub.answers(&mut out);
    assert_eq!(out, [(2, vec![Frame(vec![8; 60])])]);
    assert_eq!(hub.backlog, 0);

    // Smaller receives: more of them wait, until together they take a full queue.
    let mut hub: Hub<u32> = Hub::new(true, 0, 1514);
    hub.subscribe(1, 1);
    for k in 0..128 {
        assert_eq!(hub.receive(1, 8, || k), Ok(Answer::Later), "receive {k}");
    }
    assert_eq!(hub.receive(1, 8, || 999), Err(net::Error::InUse));
}

// Exercise the exact fixture batching arithmetic through the real codec/transport.
#[path = "../../net-test/src/batch.rs"]
mod performance_batch;

#[test]
fn nt_t_performance_batch_fits_the_actual_channel_wire_bound() {
    use moruna_svc::frame::Transport;

    fn send(count: usize, bytes: usize) -> Result<(), Transport> {
        let mut encoded = Encoder::new();
        vec![Frame(vec![0; bytes]); count]
            .encode(&mut encoded)
            .expect("encode frames");
        let (sender, receiver) = Channel::create().expect("channel");
        frame::send(&sender, Kind::Request, 4, 1, encoded)?;
        let mut received = frame::recv(&receiver).expect("receive").expect("message");
        let mut decoder = received.decoder();
        let frames = Vec::<Frame>::decode(&mut decoder).expect("decode frames");
        decoder.finish().expect("exact frame body");
        assert_eq!(frames.len(), count);
        assert!(frames.iter().all(|frame| frame.0.len() == bytes));
        Ok(())
    }

    let large = performance_batch::frames_per_send(1514, SEND_FRAMES_MAX).expect("fits");
    assert_eq!(large, 43);
    assert_eq!(send(large, 1514), Ok(()));
    assert_eq!(send(large + 1, 1514), Err(Transport::TooLarge));
    assert_eq!(send(SEND_FRAMES_MAX, 1514), Err(Transport::TooLarge));
    let small = performance_batch::frames_per_send(64, SEND_FRAMES_MAX).expect("fits");
    assert_eq!(small, SEND_FRAMES_MAX);
    assert_eq!(send(small, 64), Ok(()));
    assert_eq!(
        performance_batch::frames_per_send(usize::MAX, SEND_FRAMES_MAX),
        None
    );
    assert_eq!(
        performance_batch::frames_per_send(moruna_svc::limits::CHANNEL_MAX_BYTES, SEND_FRAMES_MAX),
        None
    );
    assert_eq!(performance_batch::frames_per_send(64, 0), None);
}
