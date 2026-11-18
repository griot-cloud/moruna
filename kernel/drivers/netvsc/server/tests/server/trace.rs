//! Work item 1.11c, with the feature `trace-first-frames` only: the card's trace reaches the
//! console's lines with the driver's own, each said as netvsc's.

use moruna_netvsc::{Netvsc, trace::Trace};
use moruna_netvsc_server::Driver;

use crate::fake::{DEADLINE, MAC, PAGE, ROOMY, parts};

#[test]
fn the_traces_lines_come_with_the_drivers_lines() {
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
        Trace::new(Box::new(|| 1_234_567)),
    )
    .unwrap_or_else(|e| panic!("the card did not start: {e}"));
    let mut driver = Driver::new(card);
    let frame = [
        &[0x02, 0, 0, 0, 0, 1][..],
        &MAC[..],
        &[0x88, 0xb5],
        &[0; 46],
    ]
    .concat();
    assert!(driver.card().send(&frame).unwrap());
    driver.card().flush().unwrap();
    host.borrow_mut().link(false);
    host.borrow_mut().flush(driver.card().channel().host());
    driver.service();
    assert!(driver.failed().is_none());
    assert_eq!(
        driver.take_lines(),
        [
            "netvsc: 1.234 ms: trace: out 1: 60 bytes in section 0, to 02:00:00:00:00:01, EtherType 0x88b5",
            "netvsc: 1.234 ms: trace: out 1 committed; the host signalled",
            "netvsc: 1.234 ms: trace: out 1 completed: status 1 (success)",
            "netvsc: 1.234 ms: trace: indication 0x4001000c (media disconnect): the link goes down",
            "netvsc: link down",
        ]
    );
    assert_eq!(host.borrow().frames, [frame]);
}
