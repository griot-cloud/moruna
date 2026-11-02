//! A configured local resolver is a real UDP peer, not a DNS shortcut.
mod common;
use common::{no_external_cards, serve};
use moruna_svc::{Channel, deadline_in};
use netstack_protocol::{Addr, Endpoint, dns, net};
use std::thread;

fn client(listener: &Channel) -> net::Client {
    let mut c = net::Client::connect(
        moruna_svc::connect(listener).unwrap(),
        deadline_in(5_000_000_000),
        moruna_svc::Account::carve(8 << 20, "net-test").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(5_000_000_000);
    c
}
#[test]
fn configured_loopback_dns_answers_both_families_and_releases_slots() {
    let config = "moruna-netstack 1
setting dns-queries 2
dns 127.0.0.1
grant listen rights=connect,listen,udp,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=4
";
    let (mut slots, server) = serve(config, &[], no_external_cards());
    let listener = slots.remove("listen").unwrap();
    let mut a = client(&listener);
    let mut b = client(&listener);
    let (socket, _) = b.bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 53)).unwrap();
    let query = thread::spawn(move || {
        for _ in 0..3 {
            assert_eq!(
                a.resolve("local.ke2.test".into()).unwrap(),
                vec![Addr::v4(127, 0, 0, 2)]
            );
        }
    });
    for _ in 0..6 {
        let (from, bytes) = b.receive_from(socket).unwrap();
        let q = dns::parse_query(&bytes).unwrap();
        assert_eq!(q.name, "local.ke2.test");
        b.send_to(
            socket,
            from,
            dns::answer(&q, true, &[Addr::v4(127, 0, 0, 2)], 0),
        )
        .unwrap();
    }
    query.join().unwrap();
    drop(b);
    drop(listener);
    drop(slots);
    server.join().unwrap();
}
