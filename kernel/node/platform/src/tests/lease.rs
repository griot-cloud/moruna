//! Work item 1.11f's defect, end to end: `platform` finds its cloud through the node's own
//! sockets (`svc::Wan`) and the real network server, whose interface `wan` takes its address
//! from a DHCP server the test holds back. On Azure (boot 1875 of 1.11c's trace) the first
//! request began 8 ms before the lease, had no route, and the next began a whole second after
//! it. A request that cannot begin for lack of an address must be made as soon as the address
//! is bound, not a retry interval later.
//!
//! The wire beside the network server stands for Azure's: the DHCP server leases 10.0.2.15/24
//! with the gateway 10.0.2.2, and a second card holds 10.0.2.2, where the instance data is
//! served. Frames to 169.254.169.254 are given to it as frames to 10.0.2.2, and its answers
//! come back from 169.254.169.254, as the host's own address translation does it.

#[path = "../../../../net/stack/tests/common/mod.rs"]
mod common;

use core::net::Ipv4Addr;
use std::{
    boxed::Box,
    format,
    string::String,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    vec::Vec,
};

use common::{MAC_A, MAC_B, Rng, Wire, dhcp_answer_offering, dhcp_request, serve};
use moruna_netstack::Stack;
use moruna_svc::now;
use netstack_protocol::{Addr, Endpoint};
use smoltcp::wire::{EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, TcpPacket};

use super::{
    RECORDED_AZURE_ID, recorded,
    svc::{PLATFORM_GRANT, client, receive, send},
};
use crate::{
    instance,
    machine::{Clock, Network, StreamError},
    svc::{Monotonic, Socket, Wan},
};

const MS: u64 = 1_000_000;

/// Where the stand-in instance data listens, behind the gateway.
const HOST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// The cloud profile's interface and `platform`'s grant as the cloud profile gives it, and
/// the card that stands for Azure's host.
fn config() -> String {
    format!(
        "moruna-netstack 1
interface wan card=net0 dhcp
interface host card=net1 ipv4=10.0.2.2/24
{PLATFORM_GRANT}grant listen-host rights=listen local=10.0.2.2/32@80 sockets=4
"
    )
}

/// Readdresses a TCP frame between instance data's address and the host's: the destination
/// of one the node sends to instance data, the source of one the host sends from port 80.
fn translate(frame: &mut [u8], to_host: bool) {
    let Ok(mut ethernet) = EthernetFrame::new_checked(&mut frame[..]) else {
        return;
    };
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return;
    }
    let Ok(mut ip) = Ipv4Packet::new_checked(ethernet.payload_mut()) else {
        return;
    };
    if ip.next_header() != IpProtocol::Tcp {
        return;
    }
    if to_host && ip.dst_addr() == instance::ADDRESS {
        ip.set_dst_addr(HOST);
    } else if !to_host
        && ip.src_addr() == HOST
        && TcpPacket::new_checked(ip.payload_mut()).is_ok_and(|tcp| tcp.src_port() == 80)
    {
        ip.set_src_addr(instance::ADDRESS);
    } else {
        return;
    }
    ip.fill_checksum();
    let (source, destination) = (ip.src_addr().into(), ip.dst_addr().into());
    if let Ok(mut tcp) = TcpPacket::new_checked(ip.payload_mut()) {
        tcp.fill_checksum(&source, &destination);
    }
}

/// The node's wire: card 0 (`wan`) and card 1 (`host`) on one link, and on it the DHCP
/// server, which answers once `answers` is set and holds the questions until then.
fn wire(answers: Arc<AtomicBool>) -> Wire {
    let mut asked: Vec<Vec<u8>> = Vec::new();
    Box::new(move |stack: &mut Stack, _: &mut Rng| {
        for mut frame in core::mem::take(&mut stack.card_mut(0).port.tx) {
            if dhcp_request(&frame).is_some() {
                asked.push(frame);
            } else {
                translate(&mut frame, true);
                stack.card_mut(1).port.deliver(frame);
            }
        }
        if answers.load(Ordering::SeqCst) {
            for frame in asked.drain(..) {
                if let Some(answer) = dhcp_answer_offering(&frame, [10, 0, 2, 15], true) {
                    stack.card_mut(0).port.deliver(answer);
                }
            }
        }
        for mut frame in core::mem::take(&mut stack.card_mut(1).port.tx) {
            translate(&mut frame, false);
            stack.card_mut(0).port.deliver(frame);
        }
    })
}

/// A connection `platform` began: when, and how it ended once it had.
type Attempt = (u64, Option<Result<(), StreamError>>);

/// Each connection `platform` began: shared with the thread that lets the lease come.
#[derive(Default)]
struct Attempts {
    made: Mutex<Vec<Attempt>>,
    changed: Condvar,
}

/// The node's network as `platform` uses it, every connection it begins noted.
struct Noted<'a> {
    wan: &'a mut Wan,
    attempts: Arc<Attempts>,
}

impl Network for Noted<'_> {
    type Stream<'s>
        = Socket<'s>
    where
        Self: 's;

    fn connect(
        &mut self,
        address: Ipv4Addr,
        port: u16,
        deadline: u64,
    ) -> Result<Socket<'_>, StreamError> {
        let index = {
            let mut made = self.attempts.made.lock().unwrap();
            made.push((now(), None));
            made.len() - 1
        };
        let met = self.wan.connect(address, port, deadline);
        self.attempts.made.lock().unwrap()[index].1 =
            Some(met.as_ref().map(|_| ()).map_err(|error| error.clone()));
        self.attempts.changed.notify_all();
        met
    }

    fn address(&mut self, deadline: u64) -> Result<Ipv4Addr, StreamError> {
        self.wan.address(deadline)
    }
}

#[test]
fn a_request_with_no_address_yet_is_made_as_soon_as_the_lease_is_taken() {
    let answers = Arc::new(AtomicBool::new(false));
    let (mut slots, server) = serve(&config(), &[MAC_A, MAC_B], wire(answers.clone()));

    // Azure's instance data, behind the gateway: it notes when the connection came.
    let mut host = client(&slots.remove("listen-host").unwrap());
    let (listening, _) = host
        .listen(Endpoint::new(Addr::v4(10, 0, 2, 2), 80), 4)
        .expect("a listener");
    let cloud = thread::spawn(move || {
        let opened = host.accept(listening).expect("a connection");
        let came = now();
        let length = instance::AZURE.encode().len();
        let mut request = Vec::new();
        while request.len() < length {
            let bytes = receive(&mut host, opened.socket);
            assert!(!bytes.is_empty(), "the request ended early");
            request.extend(bytes);
        }
        send(&mut host, opened.socket, &recorded::azure_instance());
        let _ = host.shutdown(opened.socket);
        let _ = host.close(opened.socket);
        (came, request)
    });

    // The lease comes once platform's first request has met a node with no address.
    let attempts = Arc::new(Attempts::default());
    let lease = {
        let attempts = attempts.clone();
        thread::spawn(move || {
            let mut made = attempts.made.lock().unwrap();
            while made.first().is_none_or(|(_, ended)| ended.is_none()) {
                made = attempts.changed.wait(made).unwrap();
            }
            let first = made[0].1.clone();
            drop(made);
            let at = now();
            answers.store(true, Ordering::SeqCst);
            (first, at)
        })
    };

    let mut wan = Wan::new(client(&slots.remove("listen-platform").unwrap())).unwrap();
    let mut clock = Monotonic::new().unwrap();
    let started = clock.now();
    let mut noted = Noted {
        wan: &mut wan,
        attempts: attempts.clone(),
    };
    let found = instance::find(&mut noted, &mut clock, started).expect("the cloud");
    assert_eq!(found.instance, RECORDED_AZURE_ID);

    let (first, leased) = lease.join().unwrap();
    let (came, request) = cloud.join().unwrap();
    // The first request could not begin: the node had no address.
    assert_eq!(first, Some(Err(StreamError::Unreachable)));
    assert_eq!(request, instance::AZURE.encode());
    // The next was made as soon as the lease was taken, not a second after the first began.
    let made = attempts.made.lock().unwrap().clone();
    assert_eq!(made.len(), 2, "{made:?}");
    assert!(
        came - leased < 500 * MS,
        "the request reached instance data {} ms after the lease came, and the first began {} \
         ms before it",
        (came - leased) / MS,
        (leased - made[0].0) / MS
    );
    drop(wan);
    drop(slots);
    server.join().unwrap();
}
