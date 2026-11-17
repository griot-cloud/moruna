//! A job's network (system design 3D, item 3.4) against the network server's own logic
//! (`moruna-netstack`) over real smoltcp loopback: node-admission's client mints a job's
//! channel, and the job reaches the hosts it lists at 443 and nothing else.

use std::{collections::BTreeMap, rc::Rc, thread};

use moruna_netstack::{Shared, Stack, after_turn, factory, parse};
use moruna_node::egress::{self, EgressError, MintError, PORT};
use moruna_svc::{CallError, Channel, Dispatcher, Turn, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, Policy, Target, dns, net, rights};

/// The network server of these tests: a loopback resolver it asks at 127.0.0.1; the slot
/// `listen-admission` as the cloud profile gives node-admission its client (every name at
/// 443, `grant` and `connect`: system design 3D); two narrower slots, to show a mint is
/// bounded by its minting client; and the slot `listen-peer` for the test's own server and
/// resolver.
const CONFIG: &str = "moruna-netstack 1
setting dns-queries 4
dns 127.0.0.1
grant listen-admission rights=grant,connect local=any names=*@443 sockets=64
grant listen-narrow rights=grant,connect local=any names=*.objects.test@443,store.example.test@443 sockets=64
grant listen-grantonly rights=grant local=any sockets=64
grant listen-peer rights=connect,listen,udp,resolve remote=127.0.0.0/8 local=127.0.0.0/8 sockets=16
";

/// Serves `CONFIG` on a thread; the client end of every slot's listener.
fn serve() -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(CONFIG).expect("the configuration");
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (o, t) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), o);
        theirs.push((t, policy.clone()));
    }
    let join = thread::spawn(move || {
        let stack = Stack::new(&config, &[], 0x5eed, now()).expect("the stack");
        let shared = Shared::new(stack);
        let mut d = Dispatcher::new();
        for (listener, policy) in theirs {
            d.listen(listener, factory(Rc::clone(&shared), policy))
                .expect("listen");
        }
        loop {
            after_turn(&shared, &mut d, now());
            let deadline = {
                let t = now();
                let mut st = shared.stack.borrow_mut();
                st.poll_at()
                    .unwrap_or(t + 20_000_000)
                    .clamp(t, t + 20_000_000)
            };
            if d.channels() == 0 || d.turn(deadline).expect("a turn") == Turn::Empty {
                break;
            }
        }
    });
    (ours, join)
}

fn client(listener: &Channel) -> net::Client {
    let mut c = net::Client::connect(
        moruna_svc::connect(listener).unwrap(),
        deadline_in(10_000_000_000),
        moruna_svc::Account::carve(8 << 20, "egress-test").unwrap(),
    )
    .unwrap();
    c.core().set_timeout(10_000_000_000);
    c
}

fn hosts(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

fn not_minted(result: Result<Channel, MintError>) -> MintError {
    match result {
        Ok(_) => panic!("the channel was minted"),
        Err(e) => e,
    }
}

fn refused<T: std::fmt::Debug>(result: Result<T, CallError<net::Error>>) -> net::Error {
    match result {
        Err(CallError::Remote(e)) => e,
        other => panic!("expected the network server's refusal, got {other:?}"),
    }
}

#[test]
fn a_jobs_policy_is_connect_to_its_hosts_at_443_and_nothing_else() {
    let policy = egress::policy(
        &hosts(&[
            "Store.Objects.Test.",
            "other.objects.test",
            "store.objects.test",
        ]),
        8,
    )
    .unwrap();
    assert_eq!(policy.rights, rights::CONNECT);
    assert!(policy.remote.is_empty());
    assert_eq!(policy.local.len(), 2);
    assert!(policy.may_use_source(&Addr::v4(10, 0, 0, 4)));
    assert_eq!(policy.sockets, 8);
    let names: Vec<_> = policy
        .names
        .iter()
        .map(|n| (n.pattern.as_str(), n.first, n.last))
        .collect();
    assert_eq!(
        names,
        [
            ("store.objects.test", PORT, PORT),
            ("other.objects.test", PORT, PORT)
        ]
    );
    assert_eq!(policy.validate(), Ok(()));
    // What it permits, and what not.
    assert!(policy.may_connect_name("store.objects.test", 443));
    assert!(!policy.may_connect_name("store.objects.test", 80));
    assert!(!policy.may_connect_name("x.store.objects.test", 443));
    assert!(policy.may_resolve("other.objects.test"));
    assert!(!policy.may_resolve("elsewhere.test"));
    assert!(!policy.may_connect(&Endpoint::new(Addr::v4(127, 0, 0, 1), 443)));
    assert!(!policy.may_listen(&Endpoint::new(Addr::v4(127, 0, 0, 1), 443)));
    assert!(!policy.has(rights::GRANT));
}

#[test]
fn hosts_that_make_no_policy_are_refused_by_name() {
    assert_eq!(egress::policy(&[], 8), Err(EgressError::NoHosts));
    assert_eq!(
        egress::policy(&hosts(&["a.test"]), 0),
        Err(EgressError::NoSockets)
    );
    for host in [
        "10.0.0.5", "::1", "", "a b.test", "-a.test", "a..test", "*.test", "*",
    ] {
        assert_eq!(
            egress::policy(&hosts(&["ok.test", host]), 8),
            Err(EgressError::NotAName(host.to_string())),
            "{host:?}"
        );
    }
    let many: Vec<String> = (0..65).map(|i| format!("h{i}.test")).collect();
    assert_eq!(egress::policy(&many, 8), Err(EgressError::TooMany(65)));
    // Sixty-four is a policy, and the same name twice is one rule.
    assert!(egress::policy(&many[..64], 8).is_ok());
    let twice = hosts(&["a.test", "A.TEST", "a.test."]);
    assert_eq!(egress::policy(&twice, 8).unwrap().names.len(), 1);
}

#[test]
fn admission_mints_a_jobs_channel_and_the_job_reaches_its_host_only() {
    let (mut slots, server) = serve();
    let peer_listener = slots.remove("listen-peer").unwrap();
    let admission_listener = slots.remove("listen-admission").unwrap();
    // The test's peer: an HTTPS port on 127.0.0.1, and the resolver the server asks.
    let mut server_side = client(&peer_listener);
    let (listening, _) = server_side
        .listen(Endpoint::new(Addr::v4(127, 0, 0, 1), PORT), 4)
        .unwrap();
    let mut resolver = client(&peer_listener);
    let (udp, _) = resolver
        .bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 53))
        .unwrap();
    // Answers every question until none comes for two seconds; the names asked.
    resolver.core().set_timeout(2_000_000_000);
    let dns = thread::spawn(move || {
        let mut asked = Vec::new();
        while let Ok((from, bytes)) = resolver.receive_from(udp) {
            let question = dns::parse_query(&bytes).unwrap();
            let answer = dns::answer(&question, true, &[Addr::v4(127, 0, 0, 1)], 0);
            asked.push(question.name);
            resolver.send_to(udp, from, answer).unwrap();
        }
        (resolver, asked)
    });

    // node-admission mints the job's channel.
    let mut admission = client(&admission_listener);
    let job_listener = egress::mint(&mut admission, &hosts(&["store.objects.test"]), 4).unwrap();
    let mut job = client(&job_listener);

    // The host it lists, at 443: resolved and connected.
    let opened = job
        .open(Target::Name("store.objects.test".into(), PORT), None)
        .unwrap();
    let accepted = server_side.accept(listening).unwrap();
    assert_eq!(accepted.remote, opened.local);

    // Anything else is refused before the server does anything.
    for target in [
        Target::Name("other.objects.test".into(), PORT),
        Target::Name("store.objects.test".into(), 80),
        Target::Name("x.store.objects.test".into(), PORT),
        Target::Endpoint(Endpoint::new(Addr::v4(127, 0, 0, 1), PORT)),
    ] {
        assert_eq!(
            refused(job.open(target.clone(), None)),
            net::Error::NotPermitted,
            "{target:?}"
        );
    }
    assert_eq!(
        refused(job.resolve("other.objects.test".into())),
        net::Error::NotPermitted
    );
    assert_eq!(
        refused(job.listen(Endpoint::new(Addr::v4(127, 0, 0, 1), 0), 1)),
        net::Error::NotPermitted
    );
    assert_eq!(
        refused(job.bind(Endpoint::new(Addr::v4(127, 0, 0, 1), 0))),
        net::Error::NotPermitted
    );
    // The job cannot widen its own channel.
    assert_eq!(
        refused(job.grant(Policy {
            rights: rights::CONNECT,
            remote: Vec::new(),
            local: Vec::new(),
            names: Vec::new(),
            sockets: 1,
        })),
        net::Error::NotPermitted
    );
    // The resolver was asked about the job's host, and about nothing else: every
    // refusal came before the server did anything.
    let (resolver, asked) = dns.join().unwrap();
    assert!(!asked.is_empty());
    assert!(asked.iter().all(|n| n == "store.objects.test"), "{asked:?}");

    drop(resolver);
    drop(job);
    drop(job_listener);
    drop(admission);
    drop(server_side);
    drop(admission_listener);
    drop(peer_listener);
    drop(slots);
    server.join().unwrap();
}

#[test]
fn admission_mints_only_what_its_own_policy_covers() {
    let (mut slots, server) = serve();
    let admission_listener = slots.remove("listen-admission").unwrap();
    let narrow_listener = slots.remove("listen-narrow").unwrap();
    let grant_only_listener = slots.remove("listen-grantonly").unwrap();
    let not_permitted = MintError::Refused(CallError::Remote(net::Error::NotPermitted));
    // node-admission's client (`*@443`) mints any job's names, up to its own sockets.
    let mut admission = client(&admission_listener);
    for listed in [
        &["a.objects.test", "b.c.objects.test"][..],
        &["store.example.test"][..],
        &["s3.eu-west-1.amazonaws.com", "acct.blob.core.windows.net"][..],
    ] {
        assert!(
            egress::mint(&mut admission, &hosts(listed), 64).is_ok(),
            "{listed:?}"
        );
    }
    assert_eq!(
        not_minted(egress::mint(&mut admission, &hosts(&["a.test"]), 65)),
        not_permitted
    );
    // A narrower client mints only under its own names.
    let mut narrow = client(&narrow_listener);
    assert!(egress::mint(&mut narrow, &hosts(&["a.objects.test"]), 4).is_ok());
    for listed in [&["elsewhere.test"][..], &["objects.test"][..]] {
        assert_eq!(
            not_minted(egress::mint(&mut narrow, &hosts(listed), 4)),
            not_permitted,
            "{listed:?}"
        );
    }
    // A client with only the `grant` right mints nothing with `connect`: a child's
    // rights must be its parent's (`Policy::covers`).
    let mut grant_only = client(&grant_only_listener);
    assert_eq!(
        not_minted(egress::mint(
            &mut grant_only,
            &hosts(&["a.objects.test"]),
            4
        )),
        not_permitted
    );
    // Hosts that make no policy never reach the server.
    assert_eq!(
        not_minted(egress::mint(&mut admission, &[], 4)),
        MintError::Policy(EgressError::NoHosts)
    );
    drop(grant_only);
    drop(narrow);
    drop(admission);
    drop(grant_only_listener);
    drop(narrow_listener);
    drop(admission_listener);
    drop(slots);
    server.join().unwrap();
}
