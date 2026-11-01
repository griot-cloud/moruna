//! NT-T1 to NT-T4 (SDD 29, k): the protocol's types on the wire, the frame checks,
//! the flow hash, and the receive hub.

use alloc::{vec, vec::Vec};

use moruna_svc::{Channel, Decoder, Encoder, Handle, Kind, Wire, frame};

use crate::{
    Answer, FRAME_MAX, FRAME_MIN, Frame, Hub, Info, Link, QUEUE_FRAMES, RECEIVE_BYTES,
    SEND_FRAMES_MAX, Stats, check, flow_hash, is_group, mac, net, octets,
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
    let mut hub: Hub<u32> = Hub::new(true, 0);
    assert_eq!(hub.receive(1, 8, || 100), Err(net::Error::NotSubscribed));
    hub.subscribe(1);
    hub.subscribe(1);
    hub.subscribe(2);
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
    // A full queue drops and counts; the other connection's does not.
    for _ in 0..QUEUE_FRAMES + 5 {
        hub.deliver(&[3; 60]);
    }
    assert_eq!(hub.stats(1).rx_dropped, 5);
    assert_eq!(hub.stats(9).rx_dropped, 0);
    // One answer never exceeds a message, however many frames are queued.
    let mut hub: Hub<u32> = Hub::new(true, 0);
    hub.subscribe(1);
    for _ in 0..100 {
        hub.deliver(&[4; 1514]);
    }
    let Ok(Answer::Now(first)) = hub.receive(1, 1024, || 0) else {
        panic!("frames were queued");
    };
    let bytes: usize = first.iter().map(|f| 4 + f.0.len()).sum();
    assert!(bytes <= RECEIVE_BYTES && bytes + 4 + 1514 > RECEIVE_BYTES);
    // The largest frame the protocol carries goes, alone, in one reply.
    let mut hub: Hub<u32> = Hub::new(true, 0);
    hub.subscribe(1);
    hub.deliver(&vec![5; FRAME_MAX]);
    hub.deliver(&vec![5; FRAME_MAX]);
    let Ok(Answer::Now(alone)) = hub.receive(1, 8, || 0) else {
        panic!("a frame was queued");
    };
    assert_eq!(alone.len(), 2);
    // A closed connection's queue, waiting receive and link waits go.
    let mut hub: Hub<u32> = Hub::new(false, 0);
    hub.subscribe(1);
    assert_eq!(hub.receive(1, 8, || 7), Ok(Answer::Later));
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
