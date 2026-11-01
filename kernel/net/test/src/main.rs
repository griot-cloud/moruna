//! The KE2 boot proof: genuine service calls over loopback and a QEMU user NIC.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use moruna_svc::{Channel, Console, Encoder, Wire, conformance, deadline_in, now, println, start};
use netstack_protocol::{Addr, Endpoint, Target, dns, net};
moruna_svc::main!(run);
const SECOND: u64 = 1_000_000_000;
fn client(listener: &Channel) -> Result<net::Client, String> {
    let channel = moruna_svc::connect(listener).map_err(|e| format!("connect: {e:?}"))?;
    let account = moruna_svc::Account::carve(8 << 20, "net-proof")
        .map_err(|e| format!("network account: {e:?}"))?;
    let mut client = net::Client::connect(channel, deadline_in(60 * SECOND), account)
        .map_err(|e| format!("hello: {e:?}"))?;
    client.core().set_timeout(60 * SECOND);
    Ok(client)
}
fn proof(out: &Console, listener: &Channel) -> Result<(), String> {
    let mut a = client(listener)?;
    let deadline = deadline_in(60 * SECOND);
    loop {
        let info = a.info().map_err(|e| format!("info: {e:?}"))?;
        let leased = info
            .interfaces
            .iter()
            .find(|i| i.name == "wan")
            .is_some_and(|i| {
                i.dhcp
                    && i.addrs.iter().any(|c| c.addr == Addr::v4(10, 0, 2, 15))
                    && i.gateway4 == Some(Addr::v4(10, 0, 2, 2))
            });
        if leased {
            if !info.dns.contains(&Addr::v4(10, 0, 2, 3)) {
                return Err(format!("DHCP DNS is missing: {:?}", info.dns));
            }
            break;
        }
        if now() >= deadline {
            return Err(String::from("DHCP lease was not installed in 60 seconds"));
        }
    }
    println!(
        out,
        "KE2 user DHCP: ok: 10.0.2.15 via 10.0.2.2, DNS 10.0.2.3"
    );
    // A real UDP DNS peer on loopback makes the proof independent of internet DNS.
    let mut dns_peer = client(listener)?;
    let (socket, _) = dns_peer
        .bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 53))
        .map_err(|e| format!("DNS bind: {e:?}"))?;
    let mut args = Encoder::new();
    String::from("peer.ke2.test")
        .encode(&mut args)
        .map_err(|e| format!("DNS args: {e:?}"))?;
    let query = a
        .core()
        .send(12, args)
        .map_err(|e| format!("DNS request: {e:?}"))?;
    for _ in 0..2 {
        let (from, bytes) = dns_peer
            .receive_from(socket)
            .map_err(|e| format!("DNS receive: {e:?}"))?;
        let q = dns::parse_query(&bytes).ok_or("malformed DNS question")?;
        if q.name != "peer.ke2.test" {
            return Err(format!("wrong DNS question: {}", q.name));
        }
        dns_peer
            .send_to(
                socket,
                from,
                dns::answer(&q, true, &[Addr::v4(10, 0, 2, 100)], 0),
            )
            .map_err(|e| format!("DNS answer: {e:?}"))?;
    }
    let mut reply = a
        .core()
        .reply(query, deadline_in(60 * SECOND))
        .map_err(|e| format!("DNS result: {e:?}"))?;
    let mut decoder = reply.decoder();
    let addresses =
        Vec::<Addr>::decode(&mut decoder).map_err(|e| format!("DNS addresses: {e:?}"))?;
    decoder
        .finish()
        .map_err(|e| format!("DNS trailer: {e:?}"))?;
    if addresses != vec![Addr::v4(10, 0, 2, 100)] {
        return Err(format!("wrong DNS addresses: {addresses:?}"));
    }
    println!(
        out,
        "KE2 user DNS: ok: peer.ke2.test = 10.0.2.100 (A and AAAA checked)"
    );
    let opened = a
        .open(Target::Endpoint(Endpoint::new(addresses[0], 47012)), None)
        .map_err(|e| format!("user TCP connect: {e:?}"))?;
    a.start_tls(opened.socket, "peer.ke2.test".to_string())
        .map_err(|e| format!("verified TLS: {e:?}"))?;
    let request = b"moruna-ke2-private-request\n";
    let source = moruna_svc::Memory::create_in(4096, &a.account().0)
        .map_err(|e| format!("TX allocation: {e:?}"))?;
    source
        .map(0, 4096, moruna_svc::map::WRITE)
        .and_then(|m| m.write(0, request))
        .map_err(|e| format!("TX fill: {e:?}"))?;
    let mut at = 0;
    while at < request.len() {
        let loan = moruna_svc::Memory(
            source
                .0
                .duplicate(
                    moruna_svc::Rights::READ
                        | moruna_svc::Rights::MAP
                        | moruna_svc::Rights::TRANSFER,
                )
                .map_err(|e| format!("TX loan: {e:?}"))?,
        );
        let n = a
            .send_from(opened.socket, loan, at as u64, (request.len() - at) as u32)
            .map_err(|e| format!("TLS send: {e:?}"))? as usize;
        if n == 0 {
            return Err(String::from("TLS send made no progress"));
        }
        at += n;
    }
    let mut received = Vec::new();
    loop {
        let destination = moruna_svc::Memory::create_in(4096, &a.account().0)
            .map_err(|e| format!("receive allocation: {e:?}"))?;
        let loan = moruna_svc::Memory(
            destination
                .0
                .duplicate(
                    moruna_svc::Rights::READ
                        | moruna_svc::Rights::WRITE
                        | moruna_svc::Rights::MAP
                        | moruna_svc::Rights::TRANSFER,
                )
                .map_err(|e| format!("receive loan: {e:?}"))?,
        );
        let count = a
            .receive_into(opened.socket, loan, 0, 1024)
            .map_err(|e| format!("TLS receive/close: {e:?}"))?;
        if count > 1024 {
            return Err("oversized receive completion".into());
        }
        let mut bytes = alloc::vec![0; count as usize];
        destination
            .map(0, 4096, moruna_svc::map::READ)
            .map_err(|e| format!("receive mapping: {e:?}"))?
            .read(0, &mut bytes)
            .map_err(|e| format!("receive read: {e:?}"))?;
        if bytes.is_empty() {
            break;
        }
        if received.len() + bytes.len() > 1024 {
            return Err(String::from("TLS peer exceeded reply bound"));
        }
        received.extend(bytes);
    }
    if received != b"moruna-ke2-verified-reply\n" {
        return Err(format!(
            "TLS peer replied with {} unexpected bytes",
            received.len()
        ));
    }
    a.shutdown(opened.socket)
        .map_err(|e| format!("TLS close: {e:?}"))?;
    println!(
        out,
        "KE2 user TLS: ok: chain, hostname, encrypted bytes and close_notify verified"
    );
    Ok(())
}
fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listener) = start::take("server").map(Channel) else {
        println!(out, "KE2 sockets: FAIL: no server listener");
        return 1;
    };
    if let Err(error) = proof(&out, &listener) {
        println!(out, "KE2 sockets: FAIL: {}", error);
        return 1;
    }
    let summary = conformance::run(
        &netstack_protocol::suite(),
        &mut conformance::Listener(listener),
        &mut |check| {
            if let Err(why) = check.result {
                println!(out, "KE2 sockets: FAIL: {}: {}", check.name, why);
            }
        },
    );
    if !summary.ok() {
        println!(out, "KE2 sockets: FAIL: {} checks failed", summary.failed);
        return 1;
    }
    println!(
        out,
        "KE2 conformance moruna.net: ok: {} checks passed", summary.passed
    );
    0
}
