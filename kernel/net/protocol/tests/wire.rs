//! KN-T1 (SDD 31, k): every value of `moruna.net` round trips through a message, and
//! what is not one of its values does not decode.

use moruna_svc::{Channel, Decoder, Encoder, Kind, Wire, WireError, frame};
use netstack_protocol::{
    Addr, Cidr, Endpoint, Info, Interface, NameRule, Opened, Policy, Rule, SocketState,
    SocketStatus, Target, rights,
};

fn round<T: Wire + Clone + PartialEq + std::fmt::Debug>(v: T) {
    let mut e = Encoder::new();
    v.clone().encode(&mut e).expect("encodes");
    let (tx, rx) = Channel::create().expect("a channel");
    frame::send(&tx, Kind::Reply, 0, 1, e).expect("sent");
    let mut f = frame::recv(&rx).expect("received").expect("a frame");
    let mut d = f.decoder();
    assert_eq!(T::decode(&mut d).expect("decodes"), v);
    d.finish().expect("nothing left over");
}

fn decode<T: Wire>(body: &[u8]) -> Result<T, WireError> {
    let mut handles = Vec::new();
    let mut d = Decoder::new(body, &mut handles);
    let v = T::decode(&mut d)?;
    d.finish()?;
    Ok(v)
}

fn six(s: &str) -> Addr {
    Addr(s.parse().expect("v6"))
}

#[test]
fn kn_t1_values_round_trip() {
    let a4 = Addr::v4(10, 47, 0, 1);
    let a6 = six("fd4b:4532::1");
    round(a4);
    round(a6);
    round(Endpoint::new(a6, 443));
    round(Cidr::new(a4, 24));
    let policy = Policy {
        rights: rights::CONNECT | rights::GRANT,
        remote: vec![Rule {
            net: Cidr::new(a6, 64),
            first: 1,
            last: 2,
        }],
        local: vec![Rule::any_port(Cidr::new(a4, 32))],
        names: vec![NameRule {
            pattern: "*.ke2.test".into(),
            first: 443,
            last: 443,
        }],
        sockets: 9,
    };
    round(policy.clone());
    round(Target::Endpoint(Endpoint::new(a4, 80)));
    round(Target::Name("tls.ke2.test".into(), 443));
    round(Info {
        interfaces: vec![Interface {
            name: "a".into(),
            mac: 0x5254_004b_4502,
            mtu: 1500,
            up: true,
            dhcp: false,
            addrs: vec![Cidr::new(a4, 24), Cidr::new(a6, 64)],
            gateway4: Some(Addr::v4(10, 47, 0, 254)),
            gateway6: None,
        }],
        dns: vec![a4],
        policy,
    });
    round(Opened {
        socket: 3,
        local: Endpoint::new(a4, 49152),
        remote: Endpoint::new(a4, 80),
    });
    round(SocketStatus {
        state: SocketState::CloseWait,
        local: Endpoint::new(a4, 1),
        remote: None,
        send_queued: 1,
        send_capacity: 2,
        receive_queued: 3,
        receive_capacity: 4,
        backlog: 5,
    });
    assert_eq!(a6.to_string(), "fd4b:4532::1");
    assert_eq!(Endpoint::new(a6, 443).to_string(), "[fd4b:4532::1]:443");
    assert_eq!(Cidr::new(a4, 24).to_string(), "10.47.0.1/24");
}

#[test]
fn kn_t1_what_is_not_a_value_does_not_decode() {
    assert_eq!(decode::<Addr>(&[5, 1, 2, 3, 4]), Err(WireError::Invalid));
    assert_eq!(decode::<Addr>(&[4, 1, 2, 3]), Err(WireError::Truncated));
    assert_eq!(decode::<Addr>(&[6, 0, 0]), Err(WireError::Truncated));
    assert_eq!(
        decode::<Cidr>(&[4, 10, 0, 0, 0, 33]),
        Err(WireError::Invalid)
    );
    assert!(decode::<Cidr>(&[4, 10, 0, 0, 0, 32]).is_ok());
    let mut v6 = vec![6];
    v6.extend([0; 16]);
    v6.push(129);
    assert_eq!(decode::<Cidr>(&v6), Err(WireError::Invalid));
    assert_eq!(decode::<Target>(&[2]), Err(WireError::Invalid));
    assert_eq!(decode::<SocketState>(&[12]), Err(WireError::Invalid));
    assert_eq!(
        decode::<Addr>(&[4, 1, 2, 3, 4, 0]),
        Err(WireError::Trailing)
    );
}

#[test]
fn caller_owned_ingress_major_explicitly_refuses_legacy_payload_protocol() {
    let mut hello = netstack_protocol::net::ID.hello();
    hello.major = 1;
    assert_eq!(
        netstack_protocol::net::ID.accepts(&hello),
        Err(moruna_svc::Refusal::MajorMismatch)
    );
    assert_eq!(netstack_protocol::net::ID.major, 2);
    assert_eq!(netstack_protocol::net::ID.minor, 0);
}
