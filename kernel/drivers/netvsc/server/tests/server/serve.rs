//! What the driver serves: the card's address, MTU and link; frames out and in; sends that
//! wait for the host; the link's changes; what the host sends wrong; a channel the host
//! takes back.

use std::{thread, time::Duration};

use moruna_netvsc::NetvscError;
use moruna_netvsc_server::{describe, late, not_started};
use moruna_svc::{CallError, Channel, deadline_in};
use net_protocol::{Frame, Info, net};
use netvsp_protocol::protocol::Status;

use crate::{
    World,
    fake::{MAC, PAGE, ROOMY, Random, SMALL, Settings, assert_clean},
};

const SECOND: u64 = 1_000_000_000;

/// A client of the card, through one of its listeners.
pub fn client(listener: &Channel) -> net::Client {
    let channel = moruna_svc::connect(listener).expect("connect");
    let mut client = net::Client::connect(channel, deadline_in(10 * SECOND)).expect("hello");
    client.core().set_timeout(10 * SECOND);
    client
}

/// Frames a client may send: random bytes from a station's address.
fn frames(seed: u64, lens: impl Iterator<Item = usize>) -> Vec<Vec<u8>> {
    let mut random = Random(seed);
    lens.map(|len| random.frame(len)).collect()
}

fn wire(frames: &[Vec<u8>]) -> Vec<Frame> {
    frames.iter().cloned().map(Frame).collect()
}

/// Receives until `n` frames have arrived.
fn collect(client: &mut net::Client, n: usize) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    while got.len() < n {
        got.extend(
            client
                .receive(64)
                .expect("receive")
                .into_iter()
                .map(|f| f.0),
        );
    }
    got
}

#[test]
fn the_card_is_served_with_its_address_its_mtu_and_its_link() {
    let (mut world, _) = World::new(ROOMY, |_| ());
    let listener = world.listener(0);
    let (info, link) = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        (c.info().unwrap(), c.link().unwrap())
    }));
    assert_eq!(
        info,
        Info {
            mac: net_protocol::mac(MAC),
            mtu: 1500,
            max_frame: 1514,
            offloads: 0,
            queues: 1,
        }
    );
    assert!(link.up);
    assert_eq!(link.speed_mbps, 0, "the card says no speed");
    let mut driver = world.driver(0).borrow_mut();
    assert_eq!(
        describe(driver.card()),
        "address 00:15:5d:01:02:03, MTU 1500, link up, no offloads, NVSP 6.1, NDIS 6.30"
    );
    assert!(driver.failed().is_none());
}

#[test]
fn a_start_that_runs_past_its_deadline_names_the_step_it_stopped_at() {
    // The host never answers RNDIS initialisation: the card's start ends at its deadline,
    // and the fatal line says where, as 1B's first fatal line must.
    let mut parts = crate::fake::parts(16 * PAGE, 16 * PAGE, ROOMY);
    parts.host.borrow_mut().mangle = crate::fake::on(
        crate::fake::Step::Rndis(netvsp_protocol::rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT),
        0,
        |_, _| crate::fake::Fate::Drop,
    );
    parts.host.borrow_mut().flush(parts.channel.host());
    let e = moruna_netvsc::Netvsc::start(
        parts.channel,
        parts.receive,
        parts.send,
        Some(crate::fake::DEADLINE),
    )
    .err()
    .expect("a start with no answer");
    assert!(
        matches!(e, NetvscError::Timeout("RNDIS initialisation")),
        "{e}"
    );
    assert_eq!(
        not_started("7ced8d9f", e),
        "no answer to RNDIS initialisation within 4 s"
    );
    assert_eq!(
        late("the receive buffer's GPADL"),
        "no answer to the receive buffer's GPADL within 4 s"
    );
    // Anything else names the card and the error.
    assert_eq!(
        not_started("7ced8d9f", NetvscError::Version),
        "card 7ced8d9f: the host accepts no NVSP version from 5 up"
    );
}

#[test]
fn frames_out_reach_the_host_in_order_and_are_counted() {
    let (mut world, _) = World::new(ROOMY, |_| ());
    let listener = world.listener(0);
    let out = frames(0x0b, (0..64).map(|i| 14 + i * 23));
    let largest = frames(0x0c, [1514].into_iter());
    let (batch, one) = (wire(&out), wire(&largest));
    let (sent, stats) = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        let sent = c.send(batch).unwrap() + c.send(one).unwrap();
        (sent, c.stats().unwrap())
    }));
    assert_eq!(sent, 65);
    let all: Vec<Vec<u8>> = out.into_iter().chain(largest).collect();
    assert!(
        world.host(0).borrow().frames == all,
        "a frame changed or was lost"
    );
    assert_eq!(stats.tx_frames, 65);
    assert_eq!(
        stats.tx_bytes,
        all.iter().map(|f| f.len() as u64).sum::<u64>()
    );
    assert_eq!((stats.tx_errors, stats.rx_errors), (0, 0));
    assert_clean(world.host(0));
}

#[test]
fn a_send_of_many_frames_signals_the_host_once() {
    // Sections for 42 frames of 6,144 bytes: the whole batch goes at once.
    let settings = Settings {
        receive_buffer: 64 * PAGE,
        send_buffer: 64 * PAGE,
    };
    let (mut world, _) = World::new(settings, |_| ());
    let listener = world.listener(0);
    let out = frames(0x51, (0..40).map(|i| 60 + i * 30));
    let batch = wire(&out);
    let before = world.driver(0).borrow_mut().card().channel().host_signals();
    let sent = world.serve(thread::spawn(move || client(&listener).send(batch)));
    assert_eq!(sent, Ok(40));
    let signals = world.driver(0).borrow_mut().card().channel().host_signals() - before;
    assert_eq!(signals, 1, "forty frames, one commit, one signal");
    assert!(
        world.host(0).borrow().frames == out,
        "a frame changed or was lost"
    );
    assert_clean(world.host(0));
}

#[test]
fn frames_in_reach_every_subscribed_connection() {
    let (mut world, commands) = World::new(ROOMY, |_| ());
    let (a, b, c) = (world.listener(0), world.listener(0), world.listener(0));
    let free = world.host(0).borrow().free_sections();
    let frames = frames(0x1b, (0..40).map(|i| 60 + i * 36));
    let delivered = frames.clone();
    let (got_a, got_b, stats, idle) = world.serve(thread::spawn(move || {
        let (mut a, mut b, mut idle) = (client(&a), client(&b), client(&c));
        a.subscribe().unwrap();
        b.subscribe().unwrap();
        commands.to(0, move |host, _| {
            for frame in &delivered {
                host.inject(frame);
            }
        });
        let got_a = collect(&mut a, 40);
        let got_b = collect(&mut b, 40);
        (got_a, got_b, a.stats().unwrap(), idle.stats().unwrap())
    }));
    assert_eq!(got_a, frames);
    assert_eq!(got_b, frames);
    assert_eq!(stats.rx_frames, 40);
    assert_eq!(
        stats.rx_bytes,
        frames.iter().map(|f| f.len() as u64).sum::<u64>()
    );
    assert_eq!((stats.rx_dropped, idle.rx_dropped), (0, 0));
    // Every receive section went back to the host.
    assert_eq!(world.host(0).borrow().free_sections(), free);
    assert_clean(world.host(0));
}

#[test]
fn a_send_waits_for_the_hosts_completions_and_its_frames_go_in_order() {
    // The small buffers hold two send sections, and the host keeps its completions.
    let (mut world, commands) = World::new(SMALL, |host| host.hold = true);
    let listener = world.listener(0);
    let out = frames(0x4a, (0..5).map(|i| 100 + i));
    let batch = wire(&out);
    let answer = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        let send = thread::spawn(move || c.send(batch));
        thread::sleep(Duration::from_millis(100));
        assert!(
            !send.is_finished(),
            "two frames went, three wait for a section"
        );
        commands.to(0, |host, _| host.release());
        send.join().unwrap()
    }));
    assert_eq!(answer, Ok(5));
    assert!(
        world.host(0).borrow().frames == out,
        "a frame changed or was lost"
    );
    assert_eq!(world.driver(0).borrow().server().waiting(), 0);
    assert_clean(world.host(0));
}

#[test]
fn frames_wait_for_room_on_the_ring_to_the_host_and_go_in_order() {
    // The ring to the host is one page, which holds 63 frames' packets; the send buffer
    // holds 128 sections of 2,048 bytes, so it is the ring that runs out.
    let settings = Settings {
        receive_buffer: 8 * PAGE,
        send_buffer: 64 * PAGE,
    };
    let (mut world, commands) =
        World::with_rings(PAGE, 16 * PAGE, settings, |host| host.send_section = 2048);
    let listener = world.listener(0);
    let out = frames(0x63, (0..64).map(|i| 100 + i));
    let batch = wire(&out);
    let answer = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        // The host stops reading, and will answer nothing when it reads again: the room on
        // the ring, which the host signals, is all that sends the last frame.
        commands.to(0, |host, _| {
            host.paused = true;
            host.hold = true;
        });
        let send = thread::spawn(move || c.send(batch));
        thread::sleep(Duration::from_millis(100));
        assert!(!send.is_finished(), "the ring is full");
        commands.to(0, |host, ring| {
            host.paused = false;
            host.run(ring);
        });
        let answer = send.join().unwrap();
        commands.to(0, |host, _| host.release());
        answer
    }));
    assert_eq!(answer, Ok(64));
    assert!(
        world.host(0).borrow().frames == out,
        "a frame changed or was lost"
    );
    assert_clean(world.host(0));
}

#[test]
fn the_send_indirection_table_is_ignored_and_said_once() {
    let (mut world, commands) = World::new(ROOMY, |_| ());
    world.serve(thread::spawn(move || {
        commands.to(0, |host, _| host.send_table(None));
        commands.to(0, |host, _| host.send_table(None));
    }));
    let mut driver = world.driver(0).borrow_mut();
    assert!(driver.failed().is_none());
    assert_eq!(driver.refused(), 0);
    assert_eq!(
        driver.take_lines(),
        ["netvsc: send indirection table of 16 entries ignored: one channel is served"]
    );
}

#[test]
fn a_link_change_answers_the_connections_waiting_for_one_and_is_said() {
    let (mut world, commands) = World::new(ROOMY, |_| ());
    let listener = world.listener(0);
    let (first, down, up) = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        let first = c.link().unwrap();
        let waiting = thread::spawn(move || {
            let down = c.link_changed(first.generation);
            (c, down)
        });
        thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished(), "the link has not changed");
        commands.to(0, |host, _| host.link(false));
        let (mut c, down) = waiting.join().unwrap();
        let down = down.unwrap();
        commands.to(0, |host, _| host.link(true));
        let up = c.link_changed(down.generation).unwrap();
        (first, down, up)
    }));
    assert!(first.up && !down.up && up.up);
    assert_eq!(
        (down.generation, up.generation),
        (first.generation + 1, first.generation + 2)
    );
    let lines = world.driver(0).borrow_mut().take_lines();
    assert_eq!(lines, ["netvsc: link down", "netvsc: link up"]);
}

#[test]
fn what_the_host_sends_wrong_is_counted_and_said_and_the_card_goes_on() {
    let (mut world, commands) = World::new(SMALL, |_| ());
    let listener = world.listener(0);
    let out = frames(0x20, [300].into_iter());
    let inbound = frames(0x21, [400].into_iter());
    let (sent, got, stats) = {
        let (out, inbound) = (wire(&out), inbound.clone());
        world.serve(thread::spawn(move || {
            let mut c = client(&listener);
            c.subscribe().unwrap();
            // Twenty messages no host sends: each is refused.
            commands.to(0, |host, _| {
                for _ in 0..20 {
                    host.raw_message(None, vec![0xee; 8]);
                }
            });
            // A frame the host could not send.
            commands.to(0, |host, _| host.send_status = Status::FAILURE);
            let sent = c.send(out).unwrap();
            commands.to(0, |host, _| host.send_status = Status::SUCCESS);
            // The card goes on: a frame comes in.
            commands.to(0, move |host, _| host.inject(&inbound[0]));
            let got = collect(&mut c, 1);
            (sent, got, c.stats().unwrap())
        }))
    };
    assert_eq!(sent, 1);
    assert_eq!(got, inbound);
    assert_eq!((stats.rx_errors, stats.tx_errors), (20, 1));
    let mut driver = world.driver(0).borrow_mut();
    assert_eq!(driver.refused(), 20);
    assert!(driver.failed().is_none());
    // The first sixteen refusals are said; the rest only counted.
    let lines = driver.take_lines();
    assert_eq!(lines.len(), 16, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|l| l.starts_with("netvsc: refused the host sent")),
        "{lines:?}"
    );
}

#[test]
fn a_channel_the_host_takes_back_fails_the_driver_and_sends_are_refused() {
    let (mut world, commands) = World::new(ROOMY, |_| ());
    let listener = world.listener(0);
    let out = wire(&frames(0x30, [200].into_iter()));
    let (first, second) = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        commands.to(0, |host, _| host.rescind = true);
        // The frame is answered once it is on the ring; the commit that follows finds the
        // channel closed, and the driver fails: the process ends, and the node with it.
        let first = c.send(out.clone());
        let second = c.send(out);
        (first, second)
    }));
    assert_eq!(first, Ok(1));
    assert_eq!(second, Err(CallError::Remote(net::Error::Io)));
    assert_eq!(
        world.driver(0).borrow().failed(),
        Some("sending frames: the channel is closed")
    );
}

/// The process's meter (work item 6.3ab): its line says what moved each way, the card's and
/// the protocol's counts and the process's own, once frames have moved, and then at most every
/// period while they move.
#[test]
fn the_meter_says_what_moved_at_most_every_period() {
    use moruna_netvsc_server::{Beside, METER_PERIOD_NS};
    let (mut world, commands) = World::new(ROOMY, |_| ());
    // Nothing has moved: no line.
    assert_eq!(world.driver(0).borrow_mut().meter(0, Beside::default), None);
    let listener = world.listener(0);
    let out = frames(0x6a, (0..10).map(|i| 60 + i * 100));
    let back = frames(0x6b, (0..20).map(|i| 1514 - i * 50));
    let (batch, delivered) = (wire(&out), back.clone());
    let got = world.serve(thread::spawn(move || {
        let mut c = client(&listener);
        c.subscribe().unwrap();
        assert_eq!(c.send(batch).unwrap(), 10);
        commands.to(0, move |host, _| {
            for frame in &delivered {
                host.inject(frame);
            }
        });
        collect(&mut c, 20)
    }));
    assert_eq!(got, back);
    let mut driver = world.driver(0).borrow_mut();
    // A wake and a turn of the loop, as the node's process makes them.
    let woke = driver.now();
    driver.woken();
    driver.turned(woke);
    let p = driver.process;
    assert_eq!((p.wakes, p.turns), (1, 1));
    assert!(
        p.requests >= 3,
        "the subscribe, the send, the receives: {p:?}"
    );
    let bytes = |f: &[Vec<u8>]| f.iter().map(|f| f.len()).sum::<usize>();
    let beside = Beside {
        cpu_ns: Some(7_000),
        heap_maps: Some(8),
    };
    let line = driver.meter(5_000_000, || beside).expect("frames moved");
    assert!(
        line.starts_with("netvsc: meter at_ms=5 cpu_us=7 heap_maps=8 busy_us="),
        "{line}"
    );
    for part in [
        format!(" in={}f/{}B ", back.len(), bytes(&back)),
        format!(" out={}f/{}B ", out.len(), bytes(&out)),
        String::from(" turns=1 wakes=1 "),
        String::from(" dropped=0 rx_errors=0 tx_errors=0"),
    ] {
        assert!(line.contains(&part), "{part:?} in {line}");
    }
    let card = driver.card().counts();
    assert!(
        card.receives > 0 && card.commits > 0 && card.arms > 0,
        "{card:?}"
    );
    let hub = &driver.server().hub;
    assert!(hub.answered > 0 && hub.answered_frames == 20, "{line}");
    // Within the period: nothing; past it, with nothing more moved: nothing.
    assert_eq!(
        driver.meter(6_000_000, || panic!("read only for a line")),
        None
    );
    assert_eq!(driver.meter(5_000_000 + METER_PERIOD_NS, || beside), None);
    // Past it, with a frame more: the next line.
    assert!(driver.card().send(&out[0]).unwrap());
    let next = driver
        .meter(5_000_000 + METER_PERIOD_NS, Beside::default)
        .expect("a frame moved");
    assert!(
        next.contains(" out=11f/") && next.contains("cpu_us=unknown heap_maps=unknown"),
        "{next}"
    );
}
