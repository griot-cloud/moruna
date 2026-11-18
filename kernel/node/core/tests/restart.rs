//! The service the supervisor starts again (item 2.2c): node.conf's `restart` record, its policy
//! read as network.conf reads a grant's, the window of deaths, the console's lines, the message
//! that hands `platform` each start's channel, and the supervisor's grant minting the restarted
//! service's listener from the real network server.

use std::{collections::BTreeMap, rc::Rc, string::String, thread, vec::Vec};

use moruna_netstack::{Shared, Stack, after_turn, factory, parse};
use moruna_node::{
    Bootstrap, Config, ConfigError, Lifecycle, Restart, State,
    restart::{
        API_STARTED, Deaths, PLATFORM_SLOT, RESTARTS_MAX, SUPERVISOR, Verdict, WINDOW_NS,
        api_started, ended_line, fatal_line, parse_policy, restarted_line,
    },
};
use moruna_svc::{Account, CallError, Channel, Dispatcher, Turn, deadline_in, now};
use netstack_protocol::{Addr, Endpoint, Policy, net, rights};

const SECOND: u64 = 1_000_000_000;
const MINUTE: u64 = 60 * SECOND;

/// The policy of api's listener: item 1.3b's grant for it, which the cloud profile's record
/// mints.
const LISTENER: &str =
    "rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000";

/// The record of the cloud profile (PM, item 2.2c), with 1.3b's policy.
const RECORD: &str = "restart api network network listen-supervisor rights=listen \
     local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000";

/// The cloud profile's node.conf, as items 1.1, 1.3, 1.4 and 2.2c make it.
const PROFILE: &str = "moruna-node 1
service devices image.devices 67108864
bootstrap devices devices
grant devices drivers drivers
service network image.netstack 67108864
service api image.api 67108864
service platform image.platform 16777216
grant network config network.config
link devices directory-net network directory
bootstrap platform platform
link network listen-platform platform network
";

fn node(extra: &str) -> Result<Config, ConfigError> {
    Config::parse(format!("{PROFILE}{extra}\n").as_bytes(), 1 << 30)
}

/// What network.conf's own parser makes of a grant with these options.
fn granted(options: &str) -> Policy {
    let config =
        parse(&format!("moruna-netstack 1\ngrant listen-x {options}\n")).expect("network.conf");
    config.grants[0].1.clone()
}

fn words(options: &str) -> Vec<&str> {
    options.split_ascii_whitespace().collect()
}

#[test]
fn the_profile_s_record_says_everything_a_start_needs() {
    let config = node(RECORD).expect("node.conf");
    let record = config.restart.expect("the record");
    assert_eq!(
        (
            record.service.as_str(),
            record.slot.as_str(),
            record.server.as_str(),
            record.granting.as_str()
        ),
        ("api", "network", "network", "listen-supervisor")
    );
    // Its policy is the one 1.3b's `listen-api` grant had, as network.conf reads it.
    assert_eq!(record.policy, granted(LISTENER));
    assert_eq!(record.policy.rights, rights::LISTEN);
    assert_eq!(
        (
            record.policy.sockets,
            record.policy.rx,
            record.policy.tx,
            record.policy.per_peer,
            record.policy.linger_ms
        ),
        (81, Some(65536), Some(65536), Some(8), Some(5000))
    );
    let at = |a, b, c, d, port| Endpoint::new(Addr::v4(a, b, c, d), port);
    assert!(record.policy.may_listen(&at(10, 0, 2, 15, 443)));
    assert!(!record.policy.may_listen(&at(10, 0, 2, 15, 444)));
    assert!(!record.policy.may_connect(&at(169, 254, 169, 254, 80)));
    // No link or grant of node.conf's reaches api: a start makes all it has.
    assert!(
        config
            .links
            .iter()
            .all(|l| l.left != "api" && l.right != "api")
    );
    assert!(config.grants.iter().all(|g| g.service != "api"));
    assert!(
        config
            .bootstraps
            .iter()
            .any(|(kind, service)| *kind == Bootstrap::Platform && service == "platform")
    );
    // Without a record, nothing restarts.
    assert_eq!(node("").unwrap().restart, None);
}

#[test]
fn a_record_s_policy_is_read_as_network_conf_reads_a_grant() {
    for options in [
        LISTENER,
        "rights=listen local=0.0.0.0/0@443 sockets=64",
        "rights=listen local=0.0.0.0/0@443",
        "rights=listen rx=1024 tx=16777216 per-peer=65536 linger=3600000",
        "rights=listen per-peer=1 linger=1",
        "rights=connect,listen,udp,ping,resolve,grant remote=any local=any",
        "rights=connect remote=10.47.0.0/24,fd4b:4532::/64@1-65535 local=10.47.0.1/32 \
         names=*.Example.test@443,b.test sockets=16",
        "sockets=1 rights=udp local=127.0.0.1/32@53 local=::1/128@53",
        "rights=listen local=0.0.0.0/0@443 sockets=65536",
    ] {
        assert_eq!(
            parse_policy(&words(options)),
            Ok(granted(options)),
            "{options}"
        );
    }
}

#[test]
fn a_record_that_is_not_one_is_refused() {
    use ConfigError::{Duplicate, Syntax};
    let policy = "rights=listen local=0.0.0.0/0@443";
    for (record, error) in [
        // Too few words, or no policy at all.
        ("restart api network network", Syntax),
        ("restart api network network listen-supervisor", Syntax),
        ("restart", Syntax),
        // Names that are not node.conf's, and a service that is its own server.
        (
            &*format!("restart API network network listen-supervisor {policy}"),
            Syntax,
        ),
        (
            &*format!("restart api network api listen-supervisor {policy}"),
            Syntax,
        ),
        // Slots a start fills itself.
        (
            &*format!("restart api console network listen-supervisor {policy}"),
            Syntax,
        ),
        (
            &*format!("restart api account network listen-supervisor {policy}"),
            Syntax,
        ),
        (
            &*format!("restart api platform network listen-supervisor {policy}"),
            Syntax,
        ),
        // Services node.conf does not have.
        (
            &*format!("restart web network network listen-supervisor {policy}"),
            Syntax,
        ),
        (
            &*format!("restart api network netstack listen-supervisor {policy}"),
            Syntax,
        ),
        // A granting slot node.conf already fills on the server.
        (
            &*format!("restart api network network directory {policy}"),
            Duplicate,
        ),
        (
            &*format!("restart api network network config {policy}"),
            Duplicate,
        ),
        // Policies network.conf would refuse.
        (
            "restart api network network listen-supervisor local=0.0.0.0/0@443",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=fly",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen local=0.0.0.0/33",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen local=0.0.0.0/0@9-8",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen sockets=0",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen sockets=65537",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen names=a..b",
            Syntax,
        ),
        (
            "restart api network network listen-supervisor rights=listen backlog=8",
            Syntax,
        ),
        // Item 1.3b's bounds, outside network.conf's ranges for them.
        (
            &*format!("restart api network network listen-supervisor {policy} rx=1023"),
            Syntax,
        ),
        (
            &*format!("restart api network network listen-supervisor {policy} tx=16777217"),
            Syntax,
        ),
        (
            &*format!("restart api network network listen-supervisor {policy} per-peer=0"),
            Syntax,
        ),
        (
            &*format!("restart api network network listen-supervisor {policy} per-peer=65537"),
            Syntax,
        ),
        (
            &*format!("restart api network network listen-supervisor {policy} linger=0"),
            Syntax,
        ),
        (
            &*format!("restart api network network listen-supervisor {policy} linger=3600001"),
            Syntax,
        ),
    ] {
        assert_eq!(node(record), Err(error), "{record}");
    }
    // One record.
    assert_eq!(node(&format!("{RECORD}\n{RECORD}")), Err(Duplicate));
    // A service that is started again takes nothing node.conf could give it only once.
    for given in [
        "link platform api api platform",
        "link network listen-api api network",
        "grant api drivers.extra drivers.extra",
        "bootstrap linux api",
        // Nor is it, or the server it mints from, one only Hyper-V runs (item 1.9c).
        "grant network root.hyperv root.hyperv",
    ] {
        assert_eq!(node(&format!("{given}\n{RECORD}")), Err(Syntax), "{given}");
    }
}

#[test]
fn a_death_is_restarted_five_times_in_ten_minutes_and_the_sixth_is_fatal() {
    let mut deaths = Deaths::new();
    // Five in a row, a second apart, each numbered.
    for n in 1..=RESTARTS_MAX {
        assert_eq!(deaths.died(n as u64 * SECOND), Verdict::Restart(n));
    }
    assert_eq!(deaths.died(6 * SECOND), Verdict::Fatal);

    // Six within ten minutes, the last just inside them, is fatal.
    let mut deaths = Deaths::new();
    for at in [0, MINUTE, 2 * MINUTE, 3 * MINUTE, 4 * MINUTE] {
        assert!(matches!(deaths.died(at), Verdict::Restart(_)));
    }
    assert_eq!(deaths.died(WINDOW_NS - 1), Verdict::Fatal);

    // The sixth ten minutes after the first: the first no longer counts.
    let mut deaths = Deaths::new();
    for at in [0, MINUTE, 2 * MINUTE, 3 * MINUTE, 4 * MINUTE] {
        assert!(matches!(deaths.died(at), Verdict::Restart(_)));
    }
    assert_eq!(deaths.died(WINDOW_NS), Verdict::Restart(RESTARTS_MAX));

    // A death every two and a half minutes, for an hour, is never fatal: at most four count.
    let mut deaths = Deaths::new();
    for n in 0..24 {
        let verdict = deaths.died(n * 150 * SECOND);
        assert_eq!(
            verdict,
            Verdict::Restart((n as usize + 1).min(4)),
            "death {n}"
        );
    }
    assert_eq!(WINDOW_NS, 10 * MINUTE);
}

#[test]
fn the_lines_are_the_item_s() {
    assert_eq!(
        restarted_line("api", 1),
        "moruna: service api restarted (1 of 5)"
    );
    assert_eq!(
        restarted_line("api", 5),
        "moruna: service api restarted (5 of 5)"
    );
    assert_eq!(
        ended_line("api"),
        "moruna: fatal api: ended 6 times in 10 minutes"
    );
    assert_eq!(
        fatal_line("api", "its listener was not granted: NotPermitted"),
        "moruna: fatal api: its listener was not granted: NotPermitted"
    );
    // 1B's form, as every other service's line: the stage is the service's name.
    assert_eq!(
        ended_line("api"),
        format!(
            "{}",
            moruna_abi::boot_line::Fatal {
                stage: "api",
                reason: "ended 6 times in 10 minutes"
            }
        )
    );
    // One line, whatever the reason holds.
    assert_eq!(
        fatal_line("api", "a\nb"),
        format!(
            "{}",
            moruna_abi::boot_line::Fatal {
                stage: "api",
                reason: "a\nb"
            }
        )
    );
}

#[test]
fn each_start_s_message_to_platform_is_exactly_api_started_and_its_channel() {
    assert_eq!(API_STARTED, b"api-started");
    assert!(api_started(b"api-started", 1));
    assert!(!api_started(b"api-started", 0));
    assert!(!api_started(b"api-started", 2));
    assert!(!api_started(b"api-restarted", 1));
    assert!(!api_started(b"api-started\0", 1));
    assert!(!api_started(b"", 1));
    // It is not the bootstrap's own message, which comes first.
    let memory = Bootstrap::platform_message(8 << 30);
    assert!(!api_started(&memory, 1));
    assert_eq!(Bootstrap::decode(API_STARTED, 1), None);
    assert_eq!(PLATFORM_SLOT, "platform");
}

#[test]
fn a_dead_service_that_restarts_is_pending_again_and_the_node_not_failed() {
    let mut lifecycle = Lifecycle::new(2);
    assert!(lifecycle.started(0));
    assert!(lifecycle.started(1));
    assert!(lifecycle.restarting(1));
    assert_eq!(lifecycle.state(1), Some(State::Pending));
    assert!(!lifecycle.failed());
    assert!(lifecycle.started(1));
    assert_eq!(lifecycle.state(1), Some(State::Running));
    // Only a running service restarts, and none once the node has failed.
    assert!(!lifecycle.restarting(5));
    assert!(lifecycle.terminated(0));
    assert!(lifecycle.failed());
    assert!(!lifecycle.restarting(1));
    let mut stopping = Lifecycle::new(1);
    assert!(stopping.started(0));
    stopping.stop();
    assert!(!stopping.restarting(0));
}

/// The network server on a thread, as its own host tests run it, with no card: the client end
/// of every granted listener, and the thread, which ends when every client has gone.
fn serve(config: &str) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(config).expect("network.conf");
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (our_end, their_end) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), our_end);
        theirs.push((their_end, policy.clone()));
    }
    let server = thread::spawn(move || {
        let stack = Stack::new(&config, &[], 0x2c2c_2c2c, now()).expect("the stack");
        let shared = Shared::new(stack);
        let mut dispatcher = Dispatcher::new();
        for (listener, policy) in theirs {
            dispatcher
                .listen(listener, factory(Rc::clone(&shared), policy))
                .expect("a listener");
        }
        loop {
            after_turn(&shared, &mut dispatcher, now());
            let at = now();
            let deadline = shared
                .stack
                .borrow_mut()
                .poll_at()
                .unwrap_or(at + 50_000_000)
                .clamp(at, at + 50_000_000);
            if dispatcher.channels() == 0
                || dispatcher.turn(deadline).expect("a turn") == Turn::Empty
            {
                break;
            }
        }
    });
    (ours, server)
}

fn client(listener: &Channel, name: &str) -> net::Client {
    let mut client = net::Client::connect(
        moruna_svc::connect(listener).expect("a connection"),
        deadline_in(5 * SECOND),
        Account::carve(1 << 20, name).expect("an account"),
    )
    .expect("the network server's welcome");
    client.core().set_timeout(5 * SECOND);
    client
}

#[test]
fn the_supervisor_s_grant_mints_the_restarted_service_s_listener() {
    // The supervisor's grant as the PM set it, beside one with the grant right alone, which
    // 2B first wrote: a grant hands out no right it does not hold (`Policy::covers`).
    let (slots, server) = serve(
        "moruna-netstack 1
grant listen-supervisor rights=grant,listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000
grant listen-grant-only rights=grant local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000
",
    );
    let record: Restart = node(RECORD).unwrap().restart.unwrap();

    let mut alone = client(&slots["listen-grant-only"], "grant-only");
    assert_eq!(
        alone.grant(record.policy.clone()).map(drop),
        Err(CallError::Remote(net::Error::NotPermitted))
    );

    // Each start mints a fresh listener from the same client, as the supervisor does.
    let mut supervisor = client(&slots["listen-supervisor"], "supervisor");
    let at = |port| Endpoint::new(Addr::v4(127, 0, 0, 1), port);
    for start in 0..3 {
        let minted = supervisor
            .grant(record.policy.clone())
            .unwrap_or_else(|e| panic!("start {start}: {e:?}"));
        let mut api = client(&minted, "api");
        // The service listens on 443 and nowhere else, and reaches out nowhere.
        let (socket, bound) = api.listen(at(443), 64).expect("a listener on 443");
        assert_eq!(bound.port, 443);
        assert_eq!(
            api.listen(at(444), 64).map(drop),
            Err(CallError::Remote(net::Error::NotPermitted))
        );
        assert_eq!(
            api.open(netstack_protocol::Target::Endpoint(at(80)), None)
                .map(drop),
            Err(CallError::Remote(net::Error::NotPermitted))
        );
        // A minted listener mints nothing: its policy has no grant right.
        assert_eq!(
            api.grant(record.policy.clone()).map(drop),
            Err(CallError::Remote(net::Error::NotPermitted))
        );
        // The service dies: its connection, and its port, go with it.
        api.close(socket).expect("close");
        drop(api);
        drop(minted);
    }
    // A policy wider than the supervisor's grant is refused: another port, more sockets,
    // larger buffers, a longer linger, more per peer, or bounds left to the server.
    let wider = |options: &str| parse_policy(&words(options)).unwrap();
    for options in [
        "rights=listen local=0.0.0.0/0@80 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000",
        "rights=listen local=0.0.0.0/0@443 sockets=82 rx=65536 tx=65536 per-peer=8 linger=5000",
        "rights=listen local=0.0.0.0/0@443 sockets=81 rx=65537 tx=65536 per-peer=8 linger=5000",
        "rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5001",
        "rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=9 linger=5000",
        "rights=listen local=0.0.0.0/0@443 sockets=64",
        "rights=listen,connect local=0.0.0.0/0@443 remote=any sockets=81 rx=65536 tx=65536 \
         per-peer=8 linger=5000",
    ] {
        assert_eq!(
            supervisor.grant(wider(options)).map(drop),
            Err(CallError::Remote(net::Error::NotPermitted)),
            "{options}"
        );
    }
    drop(alone);
    drop(supervisor);
    drop(slots);
    server.join().unwrap();
}

/// The record of the cloud profile with step 3 (item 3.2): `api` gets a fresh connection to
/// `node-admission` on its `admission` slot at each start.
const CONNECTING: &str = "restart api network network listen-supervisor admission rights=listen \
     local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000";

/// The supervisor's own link: it is the client of `node-admission`'s listener.
const SUPERVISOR_LINK: &str = "service node-admission image.admission 16777216
link node-admission listen supervisor admission";

#[test]
fn a_record_may_name_a_slot_each_start_is_connected_on_through_the_supervisor() {
    let config = node(&format!("{SUPERVISOR_LINK}\n{CONNECTING}")).expect("node.conf");
    let record = config.restart.as_ref().expect("the record");
    assert_eq!(record.connect.as_deref(), Some("admission"));
    assert_eq!(record.policy, granted(LISTENER));
    // The link is the supervisor's: no service's slot is filled on its client side.
    let link = config
        .links
        .iter()
        .find(|l| l.right == SUPERVISOR)
        .expect("the supervisor's link");
    assert_eq!(
        (
            link.left.as_str(),
            link.left_slot.as_str(),
            link.right_slot.as_str()
        ),
        ("node-admission", "listen", "admission")
    );
    assert!(config.links.iter().all(|l| l.right != "api"));
    // Without one, as before.
    assert_eq!(node(RECORD).unwrap().restart.unwrap().connect, None);
}

#[test]
fn a_connect_slot_needs_the_supervisor_s_link_and_no_other() {
    use ConfigError::{Duplicate, Syntax};
    let policy = "rights=listen local=0.0.0.0/0@443";
    let admission = "service node-admission image.admission 16777216";
    for (extra, error) in [
        // A slot with no link, and a link with no slot.
        (format!("{admission}\n{CONNECTING}"), Syntax),
        (format!("{SUPERVISOR_LINK}\n{RECORD}"), Syntax),
        (SUPERVISOR_LINK.to_string(), Syntax),
        // The link names another slot, or there are two.
        (
            format!("{admission}\nlink node-admission listen supervisor jobs\n{CONNECTING}"),
            Syntax,
        ),
        (
            format!(
                "{SUPERVISOR_LINK}\nlink node-admission second supervisor admission\n{CONNECTING}"
            ),
            Syntax,
        ),
        // Slots a start fills itself, or its listener's.
        (
            format!(
                "{admission}\nlink node-admission listen supervisor platform\nrestart api network network listen-supervisor platform {policy}"
            ),
            Syntax,
        ),
        (
            format!(
                "{admission}\nlink node-admission listen supervisor network\nrestart api network network listen-supervisor network {policy}"
            ),
            Syntax,
        ),
        (
            format!(
                "{admission}\nlink node-admission listen supervisor console\nrestart api network network listen-supervisor console {policy}"
            ),
            Syntax,
        ),
        (
            format!(
                "{admission}\nlink node-admission listen supervisor Admission\nrestart api network network listen-supervisor Admission {policy}"
            ),
            Syntax,
        ),
        // The supervisor is no service, never a server, and never the one restarted.
        (
            format!("service supervisor image.x 4096\n{SUPERVISOR_LINK}\n{CONNECTING}"),
            Syntax,
        ),
        (
            format!("{SUPERVISOR_LINK}\nlink supervisor x api y\n{CONNECTING}"),
            Syntax,
        ),
        (
            format!(
                "{SUPERVISOR_LINK}\nrestart supervisor network network listen-supervisor admission {policy}"
            ),
            Syntax,
        ),
        // The server's slot is its own once.
        (
            format!("{SUPERVISOR_LINK}\ngrant node-admission listen x\n{CONNECTING}"),
            Duplicate,
        ),
    ] {
        assert_eq!(node(&extra), Err(error), "{extra}");
    }
}

moruna_svc::protocol! {
    /// A server for the test of the supervisor's connections: one call.
    pub protocol probe {
        name = "moruna.test.probe";
        major = 1;
        minor = 0;
        requests {
            /// The connection's number, in the order the server took them.
            1 => fn which() -> u64;
        }
        events {}
        errors {}
    }
}

/// One connection of the probe: its number, and the server's count of closed ones.
struct Probe {
    number: u64,
    closed: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl probe::Server for Probe {
    fn which(&mut self, _: &mut moruna_svc::Context<'_>) -> Result<u64, probe::Error> {
        Ok(self.number)
    }
}

struct Session(probe::Serve<Probe>);

impl moruna_svc::Handler for Session {
    fn protocol(&self) -> moruna_svc::ProtocolId {
        probe::ID
    }
    fn handle(
        &mut self,
        cx: &mut moruna_svc::Context<'_>,
        ordinal: u16,
        d: &mut moruna_svc::Decoder<'_>,
    ) -> moruna_svc::Outcome {
        self.0.handle(cx, ordinal, d)
    }
    fn closed(&mut self) {
        self.0
            .0
            .closed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What the supervisor does at each start: a fresh connection through its end of the link,
/// which works, and which the server sees close when the start that held it has gone, while
/// the link itself stays for the next start.
#[test]
fn each_start_s_connection_is_fresh_and_ends_with_it() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let closed = Arc::new(AtomicU64::new(0));
    let (server_end, supervisor_end) = Channel::create().expect("the link");
    let counted = closed.clone();
    let server = thread::spawn(move || {
        let mut dispatcher = Dispatcher::new();
        let mut number = 0;
        dispatcher
            .listen(
                server_end,
                Box::new(move || {
                    number += 1;
                    Box::new(Session(probe::Serve(Probe {
                        number,
                        closed: counted.clone(),
                    }))) as Box<dyn moruna_svc::Handler>
                }),
            )
            .expect("listening");
        let _ = dispatcher.run();
    });
    for start in 1..=3u64 {
        let channel = moruna_svc::connect(&supervisor_end).expect("a fresh connection");
        let mut client =
            probe::Client::connect(channel, deadline_in(5 * SECOND)).expect("welcomed");
        assert_eq!(client.which(), Ok(start));
        // The start dies: the server sees its connection close.
        drop(client);
        let deadline = now() + 5 * SECOND;
        while closed.load(Ordering::SeqCst) < start {
            assert!(now() < deadline, "start {start}'s connection did not close");
            thread::yield_now();
        }
    }
    drop(supervisor_end);
    server.join().unwrap();
}
