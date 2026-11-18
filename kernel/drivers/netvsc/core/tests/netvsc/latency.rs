//! Work item 1.11d, with the feature `trace-first-frames` only: the first frames out, every
//! frame whether the trace says it or not, each timed from its commit to the read of its
//! completion, with when the caller last began to wait and was last woken in between; said
//! once, a line a frame, when every timed frame is complete.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use moruna_netvsc::{Netvsc, latency::TIMED, trace::Trace};

use crate::fake::{DEADLINE, Driver, Host, PAGE, ROOMY, assert_clean, parts};

/// The clock a timed card's trace reads, which the test sets.
type Clock = Rc<Cell<u64>>;

/// A card started with a trace on a clock the test sets, and its host.
fn timed() -> (Clock, Driver, Rc<RefCell<Host>>) {
    let clock = Rc::new(Cell::new(0));
    let time = clock.clone();
    let parts = parts(16 * PAGE, 16 * PAGE, ROOMY);
    let host = parts.host.clone();
    let mut channel = parts.channel;
    host.borrow_mut().flush(channel.host());
    let card = Netvsc::start_traced(
        channel,
        parts.receive,
        parts.send,
        Some(DEADLINE),
        &mut |_| {},
        Trace::new(Box::new(move || time.get())),
    )
    .unwrap_or_else(|e| panic!("the card did not start: {e}"));
    (clock, card, host)
}

/// A frame of 60 bytes the trace of the boot's path does not follow: what the timings
/// follow regardless.
fn frame(n: u8) -> Vec<u8> {
    [&[0x02, 0, 0, 0, 0, n][..], &[0; 6], &[0x88, 0xb5], &[n; 46]].concat()
}

/// The host reads the ring to it and answers.
fn host_runs(card: &mut Driver, host: &Rc<RefCell<Host>>) {
    host.borrow_mut().flush(card.channel().host());
}

/// A host that was paused reads what waited on the ring to it, and answers.
fn host_resumes(card: &mut Driver, host: &Rc<RefCell<Host>>) {
    let mut host = host.borrow_mut();
    host.paused = false;
    host.run(card.channel().host());
    host.flush(card.channel().host());
}

/// Reads everything the host has sent.
fn read_all(card: &mut Driver) {
    while card.poll().unwrap().is_some() {}
}

/// The trace's timing lines among `lines`, without their time.
fn timings(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter_map(|line| {
            line.split_once(" ms: trace: frame out ")
                .map(|(_, rest)| rest.to_string())
        })
        .collect()
}

fn trace(card: &mut Driver) -> &mut Trace {
    card.trace().expect("a traced card")
}

#[test]
fn each_frame_out_is_timed_from_its_commit_to_its_completions_read() {
    let (clock, mut card, host) = timed();
    let mut said = Vec::new();
    for n in 1..=TIMED as u8 {
        let ms = u64::from(n) * 1_000_000;
        clock.set(ms);
        assert!(card.send(&frame(n)).unwrap());
        card.flush().unwrap();
        clock.set(ms + 100_000);
        trace(&mut card).waiting();
        host_runs(&mut card, &host);
        clock.set(ms + 250_000);
        trace(&mut card).woken();
        clock.set(ms + 300_000);
        read_all(&mut card);
        said.extend(timings(card.take_trace()));
        // Nothing is said until the last timed frame is complete.
        assert_eq!(said.is_empty(), n < TIMED as u8, "after frame {n}");
    }
    assert_eq!(said.len(), TIMED as usize);
    for (n, line) in (1..).zip(&said) {
        assert_eq!(
            *line,
            format!(
                "{n}, 60 bytes, timed: committed {n}.000 (signalled); waited from {n}.100; woken {n}.250; completion read {n}.300, 0.300 ms after the commit"
            )
        );
    }
    // The timings are said once, and a frame after the timed ones is not timed.
    assert!(card.send(&frame(0xee)).unwrap());
    card.flush().unwrap();
    host_runs(&mut card, &host);
    read_all(&mut card);
    assert!(timings(card.take_trace()).is_empty());
    assert_clean(&host);
}

#[test]
fn a_commit_times_every_frame_it_carries_and_says_whether_it_signalled() {
    let (clock, mut card, host) = timed();
    // The caller waited and was woken before the commit: not for these frames.
    clock.set(500_000);
    trace(&mut card).waiting();
    trace(&mut card).woken();
    // A host that reads nothing: the second commit finds the ring not empty and does not
    // signal; the third carries two frames.
    host.borrow_mut().paused = true;
    clock.set(1_000_000);
    assert!(card.send(&frame(1)).unwrap());
    card.flush().unwrap();
    clock.set(2_000_000);
    assert!(card.send(&frame(2)).unwrap());
    card.flush().unwrap();
    clock.set(3_000_000);
    assert!(card.send(&frame(3)).unwrap());
    assert!(card.send(&frame(4)).unwrap());
    card.flush().unwrap();
    host_resumes(&mut card, &host);
    // Read with no wait and no wake since the commits: a client's turn read it.
    clock.set(7_500_000);
    read_all(&mut card);
    // The rest, one at a time, to finish the timed frames.
    for n in 5..=TIMED as u8 {
        assert!(card.send(&frame(n)).unwrap());
        card.flush().unwrap();
        host_runs(&mut card, &host);
        read_all(&mut card);
    }
    let said = timings(card.take_trace());
    assert_eq!(said.len(), TIMED as usize);
    assert_eq!(
        said[..4],
        [
            "1, 60 bytes, timed: committed 1.000 (signalled); no wait; not woken; completion read 7.500, 6.500 ms after the commit",
            "2, 60 bytes, timed: committed 2.000 (no signal); no wait; not woken; completion read 7.500, 5.500 ms after the commit",
            "3, 60 bytes, timed: committed 3.000 (no signal); no wait; not woken; completion read 7.500, 4.500 ms after the commit",
            "4, 60 bytes, timed: committed 3.000 (no signal); no wait; not woken; completion read 7.500, 4.500 ms after the commit",
        ]
    );
    assert_clean(&host);
}

#[test]
fn a_frame_not_yet_completed_holds_the_timings_back() {
    let (_, mut card, host) = timed();
    for n in 1..TIMED as u8 {
        assert!(card.send(&frame(n)).unwrap());
        card.flush().unwrap();
        host_runs(&mut card, &host);
        read_all(&mut card);
    }
    // The last timed frame stays in flight while the host reads nothing.
    host.borrow_mut().paused = true;
    assert!(card.send(&frame(TIMED as u8)).unwrap());
    card.flush().unwrap();
    read_all(&mut card);
    assert!(timings(card.take_trace()).is_empty());
    // Once it is complete, every timed frame is said.
    host_resumes(&mut card, &host);
    read_all(&mut card);
    assert_eq!(timings(card.take_trace()).len(), TIMED as usize);
    assert_clean(&host);
}
