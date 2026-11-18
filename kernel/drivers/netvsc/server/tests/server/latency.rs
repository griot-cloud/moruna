//! Work item 1.11d, with the feature `trace-first-frames` only: why 1.11c's trace read send
//! completions 10 to 26 ms after their commits on Azure.
//!
//! The process's loop (`node.rs`) services the card, writes the lines it took on the console,
//! and only then waits; the core writes a console line to the serial port a byte at a time,
//! and on Azure a byte costs about 50 us (1.11c's traces: two of netvsc's lines with nothing
//! between them, 38 to 55 us a byte). Here the loop is run as `node.rs` runs it against a host
//! that completes each frame at once, with the console's bytes on the trace's clock: the
//! completion is read when the lines written before the wait are out, so the trace reports
//! their time as the host's.

use std::{cell::Cell, rc::Rc};

use moruna_netvsc::{Netvsc, latency::TIMED, trace::Trace};
use moruna_vmbus::testing::TestChannel;

use crate::fake::{Bytes, DEADLINE, MAC, Netvsp, PAGE, ROOMY, parts};

type Driver = moruna_netvsc_server::Driver<TestChannel<Netvsp>, Bytes>;

/// What a byte on Azure's console costs, in nanoseconds.
const BYTE_NS: u64 = 50_000;

/// The time a line says, in nanoseconds: `netvsc: 12.345 ms: ...`.
fn said_at(line: &str) -> u64 {
    let time = line
        .strip_prefix("netvsc: ")
        .and_then(|rest| rest.split_once(" ms: "))
        .map(|(time, _)| time)
        .unwrap_or_else(|| panic!("a line with no time: {line}"));
    let (ms, us) = time
        .split_once('.')
        .expect("milliseconds to the microsecond");
    ms.parse::<u64>().unwrap() * 1_000_000 + us.parse::<u64>().unwrap() * 1_000
}

#[test]
fn a_completion_is_read_once_the_lines_before_the_wait_are_on_the_console() {
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
    let mut driver = Driver::new(card);
    // The process writes the lines it took on the console, which takes their bytes' time
    // (each line ends with CR LF on the serial port), and then waits.
    let mut console = Vec::new();
    let mut write_and_wait = |driver: &mut Driver| {
        let lines = driver.take_lines();
        let bytes: u64 = lines.iter().map(|line| line.len() as u64 + 2).sum();
        clock.set(clock.get() + bytes * BYTE_NS);
        console.extend(lines);
        driver.card().trace().expect("a traced card").waiting();
        bytes * BYTE_NS
    };
    // An ARP request, a frame the trace says in every design.
    let arp = [
        &[0xff; 6][..],
        &MAC[..],
        &[0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1],
        &MAC[..],
        &[10, 0, 0, 4],
        &[0; 6],
        &[10, 0, 0, 1],
    ]
    .concat();
    let mut before_wait = Vec::new();
    for n in 1..=u64::from(TIMED) {
        // The network server's frame, in a turn of the process: written and committed. The
        // fake host would answer within the signal; Azure's answers once the turn is over.
        clock.set(n * 1_000_000_000);
        host.borrow_mut().paused = true;
        assert!(driver.card().send(&arp).unwrap());
        driver.service();
        // The host completes it at once and signals.
        {
            let mut host = host.borrow_mut();
            host.paused = false;
            host.run(driver.card().channel().host());
            host.flush(driver.card().channel().host());
        }
        // The loop writes its lines, then waits; the wake channel holds the signal already.
        before_wait.push(write_and_wait(&mut driver));
        driver.card().trace().expect("a traced card").woken();
        driver.service();
        write_and_wait(&mut driver);
    }
    assert!(driver.failed().is_none());
    // The trace's completion of each frame said comes as long after its commit as the lines
    // before the wait took: about 7 ms for the first frame, with a host that took none.
    let committed: Vec<u64> = console
        .iter()
        .filter(|line| line.contains(" committed; "))
        .map(|line| said_at(line))
        .collect();
    let completed: Vec<u64> = console
        .iter()
        .filter(|line| line.contains(" completed: status 1"))
        .map(|line| said_at(line))
        .collect();
    assert!(!committed.is_empty());
    assert_eq!(committed.len(), completed.len());
    for (n, (commit, completion)) in committed.iter().zip(&completed).enumerate() {
        assert_eq!(completion - commit, before_wait[n], "frame {}", n + 1);
    }
    assert!(before_wait[0] > 7_000_000, "{} ns", before_wait[0]);
    // The timings say the same of every frame: the wait began when the lines were out, the
    // wake and the read came with it. A frame the trace no longer says took no time.
    let timings: Vec<&String> = console
        .iter()
        .filter(|line| line.contains(" ms: trace: frame out "))
        .collect();
    assert_eq!(timings.len(), TIMED as usize);
    for (n, line) in timings.iter().enumerate() {
        let commit = (n as u64 + 1) * 1_000_000_000;
        let read = commit + before_wait[n];
        let at = |ns: u64| format!("{}.{:03}", ns / 1_000_000, ns / 1_000 % 1_000);
        assert!(
            line.ends_with(&format!(
                "frame out {}, 42 bytes, timed: committed {} (signalled); waited from {}; woken {}; completion read {}, {} ms after the commit",
                n + 1,
                at(commit),
                at(read),
                at(read),
                at(read),
                at(before_wait[n]),
            )),
            "{line}"
        );
    }
    assert_eq!(host.borrow().frames.len(), TIMED as usize);
}
