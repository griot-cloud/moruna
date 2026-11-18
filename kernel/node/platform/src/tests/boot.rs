//! The whole boot against the fake clouds: the lines in their order, the hand-over `api` gets,
//! and every fatal line.

use core::net::Ipv4Addr;
use std::{
    cell::RefCell,
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use moruna_node_config::{Cloud, Handover, HandoverError, NodeConfig, base64};

use super::{
    FakeClock, FakeLayer, FakeNetwork, FakeWall, MOUNTED, RECORDED_AZURE_ID, RECORDED_CONFIG,
    RECORDED_OPENSTACK_ID, RECORDED_OPENSTACK_TIME, Reply, SECOND, Sent, answer, azure, openstack,
    recorded_health, wire_server,
};
use crate::{
    boot::{Fatal, Inputs, READY_LIMIT_NS, RETIME_NS, Stage, cloud_line, ready_line, run, serve},
    machine::{Clock, Control, Link, LinkError, StreamError},
    wireserver,
};

const MS: u64 = 1_000_000;
const USER: &str = "/openstack/latest/user_data";
const META: &str = "/openstack/latest/meta_data.json";

/// What reached the console and the link, in order.
type Events = Rc<RefCell<Vec<String>>>;

/// `api`'s end of the link, played by the test.
struct FakeApi {
    clock: FakeClock,
    events: Events,
    /// Whether the hand-over can be sent.
    send: Result<(), LinkError>,
    /// What `api` answers, after `takes` nanoseconds.
    answer: Result<Vec<u8>, LinkError>,
    takes: u64,
    /// Every message sent to `api`.
    sent: Vec<Vec<u8>>,
    /// The deadline of every call.
    deadlines: Vec<u64>,
    /// How many starts of `api` the supervisor's channel brings, and what it meets after.
    starts: usize,
    then: LinkError,
    /// The deadline of every wait for a start's channel.
    channels: Vec<u64>,
}

impl FakeApi {
    fn new(clock: &FakeClock, events: &Events) -> FakeApi {
        FakeApi {
            clock: clock.clone(),
            events: events.clone(),
            send: Ok(()),
            answer: Ok(b"ready".to_vec()),
            takes: 40 * MS,
            sent: Vec::new(),
            deadlines: Vec::new(),
            starts: usize::MAX,
            then: LinkError::Closed,
            channels: Vec::new(),
        }
    }
}

/// The supervisor's channel to `platform`, played by the test: each start of `api` it sends is
/// the one fake `api`.
struct FakeControl(Rc<RefCell<FakeApi>>);

/// One start's channel to the fake `api`.
struct ApiEnd(Rc<RefCell<FakeApi>>);

impl Link for ApiEnd {
    fn send(&mut self, message: &[u8], deadline: u64) -> Result<(), LinkError> {
        self.0.borrow_mut().send(message, deadline)
    }

    fn receive(&mut self, deadline: u64) -> Result<Vec<u8>, LinkError> {
        self.0.borrow_mut().receive(deadline)
    }
}

impl Control for FakeControl {
    type Api = ApiEnd;

    fn next_api(&mut self, deadline: u64) -> Result<ApiEnd, LinkError> {
        let mut api = self.0.borrow_mut();
        api.channels.push(deadline);
        if api.starts == 0 {
            return Err(api.then.clone());
        }
        api.starts -= 1;
        Ok(ApiEnd(self.0.clone()))
    }
}

impl Link for FakeApi {
    fn send(&mut self, message: &[u8], deadline: u64) -> Result<(), LinkError> {
        self.deadlines.push(deadline);
        self.send.clone()?;
        self.events.borrow_mut().push("the hand-over".to_string());
        self.sent.push(message.to_vec());
        Ok(())
    }

    fn receive(&mut self, deadline: u64) -> Result<Vec<u8>, LinkError> {
        self.deadlines.push(deadline);
        if self.answer == Err(LinkError::TimedOut) {
            self.clock.sleep_until(deadline);
        } else {
            self.clock.advance(self.takes);
        }
        self.answer.clone()
    }
}

/// One boot: what it returned, what reached the console and the link, `api`'s end, and the
/// image filesystem.
struct Booted {
    result: Result<Handover, Fatal>,
    events: Vec<String>,
    api: FakeApi,
    layer: FakeLayer,
    clock: FakeClock,
    /// Each request a server saw, and when its connection was made.
    requests: Vec<(u64, Sent)>,
    wall: FakeWall,
}

impl Booted {
    /// The fatal line of a boot that stopped.
    fn line(&self) -> String {
        self.result
            .as_ref()
            .expect_err("the boot stops")
            .to_string()
    }
}

/// The node's memory in these boots, as the supervisor gives it.
const MEMORY: u64 = 8 << 30;

const INPUTS: Inputs = Inputs {
    started_ns: 0,
    memory_bytes: MEMORY,
};

/// Boots against `network`, with `api` changed by `prepare` from one that answers `ready`.
fn boot_with(network: FakeNetwork, inputs: Inputs, prepare: impl FnOnce(&mut FakeApi)) -> Booted {
    boot_layered(network, inputs, prepare, |_| {})
}

/// The same, with the image filesystem changed by `layer` from one that has mounted the
/// engine's layer.
fn boot_layered(
    mut network: FakeNetwork,
    inputs: Inputs,
    prepare: impl FnOnce(&mut FakeApi),
    layer: impl FnOnce(&mut FakeLayer),
) -> Booted {
    let mut clock = network.clock.clone();
    let events = Events::default();
    let mut api = FakeApi::new(&clock, &events);
    prepare(&mut api);
    let api = Rc::new(RefCell::new(api));
    let mut fs = FakeLayer::new(&clock, &events);
    layer(&mut fs);
    let console = events.clone();
    let mut wall = FakeWall::default();
    let result = run(
        &mut network,
        &mut clock,
        &mut wall,
        &mut FakeControl(api.clone()),
        &mut fs,
        &mut |line: &str| console.borrow_mut().push(line.to_string()),
        inputs,
    );
    let events = events.borrow().clone();
    let api = Rc::try_unwrap(api)
        .ok()
        .expect("no channel to api outlives the boot")
        .into_inner();
    Booted {
        result,
        events,
        api,
        layer: fs,
        clock,
        requests: network.requests,
        wall,
    }
}

fn boot(serve: impl FnMut(u64, &Sent) -> Reply + 'static) -> Booted {
    boot_with(
        FakeNetwork::serving(&FakeClock::default(), serve),
        INPUTS,
        |_| {},
    )
}

/// OpenStack with `user_data` as the instance's.
fn openstack_with(user_data: &[u8]) -> impl FnMut(u64, &Sent) -> Reply + 'static {
    let user_data = user_data.to_vec();
    move |_, sent| match sent.path.as_str() {
        USER => Reply::Answer(answer("200 OK", &user_data)),
        _ => openstack(sent),
    }
}

/// Azure with `user_data` as the instance's, base64-encoded as Azure gives it.
fn azure_with(user_data: &[u8]) -> impl FnMut(u64, &Sent) -> Reply + 'static {
    let document = format!(
        r#"{{"compute":{{"userData":"{}","vmId":"{RECORDED_AZURE_ID}"}},"network":{{}}}}"#,
        base64::encode(user_data)
    );
    move |_, sent| {
        if sent.to.0 == crate::wireserver::ADDRESS {
            return wire_server(sent);
        }
        Reply::Answer(answer("200 OK", document.as_bytes()))
    }
}

#[test]
fn a_node_on_openstack_hands_over_and_says_it_is_ready() {
    let clock = FakeClock::default();
    let latency = clock.clone();
    let network = FakeNetwork::serving(&clock, move |_, sent| {
        latency.advance(2 * MS);
        openstack(sent)
    });
    let booted = boot_with(network, INPUTS, |_| {});
    let handover = booted.result.clone().unwrap();
    assert_eq!(
        handover,
        Handover {
            config: NodeConfig::validate(RECORDED_CONFIG.as_bytes(), MEMORY).unwrap(),
            instance: RECORDED_OPENSTACK_ID.to_string(),
            cloud: Cloud::OpenStack,
            // The Date of the answer that named the cloud, and when that answer arrived.
            wall_clock: RECORDED_OPENSTACK_TIME,
            read_at_ns: 4 * MS,
            // The node's memory the supervisor gave.
            memory_bytes: MEMORY,
            // The ready line's, for api's later starts (item 2.2c).
            boot_ms: Some(46),
        }
    );
    // The cloud's line, then the hand-over, then, once api has answered and imagefs has
    // mounted the engine's layer, the ready line: three requests of 2 ms and api's 40 ms after
    // the core's clock began.
    assert_eq!(
        booted.events,
        [
            format!("moruna: cloud openstack instance {RECORDED_OPENSTACK_ID}"),
            "the hand-over".to_string(),
            MOUNTED.to_string(),
            "moruna: ready api=10.0.2.15:443 boot_ms=46".to_string(),
        ]
    );
    // imagefs was asked with instance data's limit: 90 s from platform's start (1B).
    assert_eq!(booted.layer.deadlines, [90 * SECOND]);
    // The node's wall clock was set once, before the hand-over: to that Date as of when its
    // answer arrived, 2 ms before (system design 3D).
    assert_eq!(
        booted.wall.times(),
        [RECORDED_OPENSTACK_TIME * SECOND + 2 * MS]
    );
    // What api got is the hand-over, as its half of the crate reads it: before the node was
    // ready, so with no boot_ms.
    let first = Handover {
        boot_ms: None,
        ..handover
    };
    assert_eq!(booted.api.sent, [first.encode().unwrap()]);
    assert_eq!(Handover::decode(&booted.api.sent[0]), Ok(first));
    // api had 10 s from the cloud's line, after the three requests' 6 ms.
    assert_eq!(booted.api.deadlines, [6 * MS + 10 * SECOND; 2]);
}

/// Who was asked what, in order: the address, the method and the path.
fn asked(booted: &Booted) -> Vec<(Ipv4Addr, &str, &str)> {
    booted
        .requests
        .iter()
        .map(|(_, sent)| (sent.to.0, sent.method.as_str(), sent.path.as_str()))
        .collect()
}

const IMDS: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
const AZURE_INSTANCE: &str = "/metadata/instance?api-version=2021-02-01";

#[test]
fn a_node_on_azure_reports_ready_once_api_answers_and_the_engine_s_layer_is_mounted() {
    let clock = FakeClock::default();
    let latency = clock.clone();
    let network = FakeNetwork::serving(&clock, move |_, sent| {
        latency.advance(2 * MS);
        azure(sent)
    });
    let booted = boot_with(network, INPUTS, |_| {});
    assert!(booted.result.is_ok());
    // Instance data, then the goal state and the Health document from the WireServer.
    assert_eq!(
        asked(&booted),
        [
            (IMDS, "GET", AZURE_INSTANCE),
            (wireserver::ADDRESS, "GET", "/machine?comp=goalstate"),
            (wireserver::ADDRESS, "POST", "/machine?comp=health"),
        ]
    );
    assert_eq!(booted.requests[2].1.body, recorded_health().as_bytes());
    // The report comes after api's answer and imagefs's (1B, amended for item 1.6e): api's 10 s
    // count from instance data's 2 ms, and the WireServer is asked once both have answered,
    // 40 ms later.
    assert_eq!(booted.api.deadlines, [2 * MS + 10 * SECOND; 2]);
    assert_eq!(booted.layer.mounted_at, Some(42 * MS));
    assert_eq!(booted.requests[1].0, 42 * MS);
    assert_eq!(
        booted.events,
        [
            format!("moruna: cloud azure instance {RECORDED_AZURE_ID}"),
            "the hand-over".to_string(),
            MOUNTED.to_string(),
            "moruna: ready api=10.0.2.15:443 boot_ms=46".to_string(),
        ]
    );
}

#[test]
fn a_node_on_openstack_makes_no_ready_report() {
    let booted = boot(|_, sent| {
        assert_ne!(
            sent.to.0,
            wireserver::ADDRESS,
            "OpenStack has no WireServer"
        );
        openstack(sent)
    });
    assert!(booted.result.is_ok());
    assert!(booted.requests.iter().all(|(_, sent)| sent.to.0 == IMDS));
}

#[test]
fn a_node_on_azure_with_an_invalid_config_makes_no_ready_report() {
    let booted = boot(azure_with(br#"{"moruna":2}"#));
    assert_eq!(asked(&booted), [(IMDS, "GET", AZURE_INSTANCE)]);
    assert_eq!(booted.result.unwrap_err().stage, Stage::Config);
}

#[test]
fn a_ready_report_azure_does_not_take_is_fatal_after_2_minutes() {
    let booted = boot(|_, sent| {
        if sent.to.0 == wireserver::ADDRESS {
            return Reply::Answer(answer("503 Service Unavailable", b""));
        }
        azure(sent)
    });
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: Azure did not take the ready report within 120 s: GET \
         /machine?comp=goalstate: answered 503"
    );
    // The cloud was named, api answered and imagefs mounted the engine's layer, and the report
    // was tried for 2 minutes from then (api's 40 ms); the node never said it was ready.
    assert_eq!(
        booted.events,
        [
            format!("moruna: cloud azure instance {RECORDED_AZURE_ID}"),
            "the hand-over".to_string(),
            MOUNTED.to_string(),
        ]
    );
    assert_eq!(booted.api.sent.len(), 1);
    assert_eq!(booted.clock.now(), 40 * MS + 120 * SECOND);
    assert_eq!(booted.requests.len(), 1 + 24);
}

#[test]
fn a_node_on_azure_hands_over_and_says_it_is_ready() {
    let booted = boot(|_, sent| azure(sent));
    let handover = booted.result.clone().unwrap();
    assert_eq!(handover.cloud, Cloud::Azure);
    assert_eq!(handover.instance, RECORDED_AZURE_ID);
    assert_eq!(handover.wall_clock, RECORDED_OPENSTACK_TIME + 2);
    assert_eq!(
        handover.config,
        NodeConfig::validate(RECORDED_CONFIG.as_bytes(), MEMORY).unwrap()
    );
    assert_eq!(
        booted.events,
        [
            format!("moruna: cloud azure instance {RECORDED_AZURE_ID}"),
            "the hand-over".to_string(),
            MOUNTED.to_string(),
            "moruna: ready api=10.0.2.15:443 boot_ms=40".to_string(),
        ]
    );
    // api had it before the node was ready, so with no boot_ms; later starts get the line's.
    assert_eq!(handover.boot_ms, Some(40));
    assert_eq!(
        Handover::decode(&booted.api.sent[0]),
        Ok(Handover {
            boot_ms: None,
            ..handover
        })
    );
}

#[test]
fn each_invalid_config_is_fatal_with_its_reason() {
    let key = base64::encode(&[7; 32]);
    let nine = vec![format!("\"{key}\""); 9].join(",");
    let cases = [
        // The work item's five.
        (
            r#"{"moruna":1,"api":{}}"#.to_string(),
            "`api.token_keys` is missing",
        ),
        (
            format!(r#"{{"moruna":1,"api":{{"token_keys":[{nine}]}}}}"#),
            "`api.token_keys` holds 9 keys: 1 to 8 are allowed",
        ),
        (
            r#"{"moruna":1,"api":{"token_keys":["not-a-key"]}}"#.to_string(),
            "`api.token_keys[0]` is not the standard base64 of a 32-byte Ed25519 public key",
        ),
        (
            format!(r#"{{"moruna":1,"api":{{"token_keys":["{key}"]}},"extra":true}}"#),
            "unknown field `extra` in node config",
        ),
        (
            format!(r#"{{"moruna":2,"api":{{"token_keys":["{key}"]}}}}"#),
            "`moruna` is 2: this node reads version 1",
        ),
        // And others.
        (
            r#"{"moruna":1,"api":{"token_keys":[]}}"#.to_string(),
            "`api.token_keys` holds 0 keys: 1 to 8 are allowed",
        ),
        (
            format!(r#"{{"api":{{"token_keys":["{key}"]}}}}"#),
            "`moruna` is missing: node config says its version",
        ),
        (
            "#cloud-config\nusers: []\n".to_string(),
            "not JSON: expected value at line 1 column 1",
        ),
        (String::new(), "the instance has no user-data"),
    ];
    for (user_data, reason) in cases {
        let line = format!("moruna: fatal config: {reason}");
        // The same config is refused the same way on either cloud.
        for booted in [
            boot(openstack_with(user_data.as_bytes())),
            boot(azure_with(user_data.as_bytes())),
        ] {
            assert_eq!(booted.line(), line);
            assert_eq!(booted.result.unwrap_err().stage, Stage::Config);
            // Nothing is said of the cloud and nothing reaches api.
            assert_eq!(booted.events, [] as [&str; 0]);
            assert!(booted.api.sent.is_empty());
        }
    }
}

#[test]
fn an_instance_without_user_data_is_fatal() {
    let line = "moruna: fatal config: the instance has no user-data";
    // OpenStack answers 404 for the user-data of an instance created with none.
    let booted = boot(|_, sent| match sent.path.as_str() {
        USER => Reply::Answer(super::recorded::openstack_no_user_data()),
        _ => openstack(sent),
    });
    assert_eq!(booted.line(), line);
    // Azure's document has an empty `userData`, or none.
    for compute in [
        format!(r#"{{"vmId":"{RECORDED_AZURE_ID}","userData":""}}"#),
        format!(r#"{{"vmId":"{RECORDED_AZURE_ID}"}}"#),
    ] {
        let document = format!(r#"{{"compute":{compute}}}"#);
        let booted = boot(move |_, _| Reply::Answer(answer("200 OK", document.as_bytes())));
        assert_eq!(booted.line(), line);
    }
    // And user-data that Azure did not give as base64 is refused as that.
    let document = format!(r#"{{"compute":{{"vmId":"{RECORDED_AZURE_ID}","userData":"{{}}"}}}}"#);
    let booted = boot(move |_, _| Reply::Answer(answer("200 OK", document.as_bytes())));
    assert_eq!(
        booted.line(),
        "moruna: fatal config: the user-data is not base64"
    );
}

#[test]
fn the_function_cache_needs_the_node_s_memory() {
    let config = format!(
        r#"{{"moruna":1,"api":{{"token_keys":["{}"]}},"functions":{{"cache_bytes":268435456}}}}"#,
        base64::encode(&[7; 32])
    );
    let network = || FakeNetwork::serving(&FakeClock::default(), openstack_with(config.as_bytes()));
    let known = boot_with(network(), INPUTS, |_| {});
    assert_eq!(
        known.result.unwrap().config.functions.cache_bytes,
        256 << 20
    );
    let small = Inputs {
        memory_bytes: 256 << 20,
        ..INPUTS
    };
    assert_eq!(
        boot_with(network(), small, |_| {}).line(),
        "moruna: fatal config: `functions.cache_bytes` is 268435456: the most is 134217728, \
         half the node's memory"
    );
    // A config that leaves the cache at its default takes 1 GiB or an eighth of the node's
    // memory (PM, item 3.6), and
    // `api` is handed the memory it was judged on.
    let default = |memory_bytes| {
        let network = FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
        let inputs = Inputs {
            memory_bytes,
            ..INPUTS
        };
        let handover = boot_with(network, inputs, |_| {}).result.unwrap();
        assert_eq!(handover.memory_bytes, memory_bytes);
        handover.config.functions.cache_bytes
    };
    assert_eq!(default(MEMORY), 1 << 30);
    assert_eq!(default(1_500_000_000), 187_500_000);
}

#[test]
fn a_config_too_large_to_hand_over_is_fatal() {
    let block = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        base64::encode(&[0x30; 1500])
    );
    let key = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        base64::encode(&[0x31; 138])
    );
    let config = serde_json::json!({
        "moruna": 1,
        "api": {
            "token_keys": [base64::encode(&[7; 32])],
            "tls": { "certificate": block.repeat(32), "key": key }
        }
    })
    .to_string();
    assert!(config.len() > 64 * 1024 && config.len() < 1 << 20);
    let booted = boot(openstack_with(config.as_bytes()));
    let Err(HandoverError::TooLarge(bytes)) = (Handover {
        config: NodeConfig::validate(config.as_bytes(), MEMORY).unwrap(),
        instance: RECORDED_OPENSTACK_ID.to_string(),
        cloud: Cloud::OpenStack,
        wall_clock: RECORDED_OPENSTACK_TIME,
        read_at_ns: 0,
        memory_bytes: MEMORY,
        boot_ms: None,
    })
    .encode() else {
        panic!("the hand-over fits");
    };
    assert_eq!(
        booted.line(),
        format!(
            "moruna: fatal config: node config is too large: the hand-over is {bytes} bytes, \
             and a channel message carries 65536"
        )
    );
    assert_eq!(booted.events, [] as [&str; 0]);
}

#[test]
fn the_wall_clock_is_the_date_of_the_answer_that_named_the_cloud() {
    let named_with = |headers: &'static str| {
        boot(move |_, sent| match sent.path.as_str() {
            META => {
                let body = format!(r#"{{"uuid":"{RECORDED_OPENSTACK_ID}"}}"#);
                let head = format!(
                    "HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\n\r\n",
                    body.len()
                );
                Reply::Answer((head + &body).into_bytes())
            }
            // The user-data's own Date is another, and is not the clock.
            USER => Reply::Answer(
                format!(
                    "HTTP/1.1 200 OK\r\nDate: Thu, 01 Jan 1970 00:00:00 GMT\r\n\
                     Content-Length: {}\r\n\r\n{RECORDED_CONFIG}",
                    RECORDED_CONFIG.len()
                )
                .into_bytes(),
            ),
            _ => openstack(sent),
        })
    };
    let booted = named_with("date: Sun, 06 Nov 1994 08:49:37 GMT\r\n");
    assert_eq!(booted.result.unwrap().wall_clock, 784_111_777);

    let none = "moruna: fatal instance-data: the answer that named the cloud has no single Date \
                header";
    let not_a_date = "moruna: fatal instance-data: the Date header of the answer that named the \
                      cloud is not an IMF-fixdate";
    for (headers, line) in [
        ("", none),
        (
            "Date: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:49:38 GMT\r\n",
            none,
        ),
        ("Date:\r\n", not_a_date),
        ("Date: Sunday, 06-Nov-94 08:49:37 GMT\r\n", not_a_date),
        ("Date: Mon, 06 Nov 1994 08:49:37 GMT\r\n", not_a_date),
        ("Date: 784111777\r\n", not_a_date),
    ] {
        let booted = named_with(headers);
        assert_eq!(booted.line(), line, "{headers:?}");
        assert_eq!(booted.result.unwrap_err().stage, Stage::InstanceData);
        assert_eq!(booted.events, [] as [&str; 0]);
    }
}

#[test]
fn no_instance_data_is_fatal_at_90_s_from_platform_s_start() {
    let clock = FakeClock::default();
    clock.advance(3 * SECOND);
    let network = FakeNetwork::serving(&clock, |_, _| panic!("no server"))
        .connecting(|_| Err((StreamError::Refused, 0)));
    let inputs = Inputs {
        started_ns: 3 * SECOND,
        ..INPUTS
    };
    let booted = boot_with(network, inputs, |_| {});
    assert_eq!(
        booted.line(),
        "moruna: fatal instance-data: nothing by 90 s: GET \
         /metadata/instance?api-version=2021-02-01: connection refused"
    );
    assert_eq!(booted.clock.now(), 93 * SECOND);
    assert_eq!(booted.events, [] as [&str; 0]);
}

#[test]
fn an_api_that_does_not_answer_ready_is_fatal() {
    let network = || FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
    let cloud = format!("moruna: cloud openstack instance {RECORDED_OPENSTACK_ID}");

    // It answers something else.
    for other in [&b"READY"[..], b"ready\n", b"", b"{\"ready\":true}"] {
        let booted = boot_with(network(), INPUTS, |api| api.answer = Ok(other.to_vec()));
        assert_eq!(
            booted.line(),
            "moruna: fatal platform: api answered the hand-over with something else"
        );
        // The cloud was named and the hand-over sent; the node never said it was ready.
        assert_eq!(booted.events, [cloud.clone(), "the hand-over".to_string()]);
        assert_eq!(booted.result.unwrap_err().stage, Stage::Platform);
    }

    // It closes its end, before the hand-over or after it.
    let booted = boot_with(network(), INPUTS, |api| api.send = Err(LinkError::Closed));
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: the hand-over was not sent to api: api closed its end of the \
         link"
    );
    assert_eq!(booted.events, [cloud.as_str()]);
    let booted = boot_with(network(), INPUTS, |api| api.answer = Err(LinkError::Closed));
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: api did not answer: api closed its end of the link"
    );
    let booted = boot_with(network(), INPUTS, |api| {
        api.answer = Err(LinkError::Other(
            "the kernel refused: bad handle".to_string(),
        ));
    });
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: api did not answer: the kernel refused: bad handle"
    );

    // An api that says nothing is fatal when its 10 s have passed (PM, 2026-11-18).
    let booted = boot_with(network(), INPUTS, |api| {
        api.answer = Err(LinkError::TimedOut);
    });
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: api did not answer ready within 10 s"
    );
    assert_eq!(booted.clock.now(), 10 * SECOND);
    assert_eq!(booted.api.deadlines, [10 * SECOND, 10 * SECOND]);
    assert_eq!(READY_LIMIT_NS, 10 * SECOND);
    // And one that answers within them is ready.
    let booted = boot_with(network(), INPUTS, |api| api.takes = 10 * SECOND - 1);
    assert_eq!(booted.events.len(), 4);
    assert_eq!(
        booted.events[3],
        format!("moruna: ready api=10.0.2.15:443 boot_ms={}", 10_000 - 1)
    );
    assert!(booted.result.is_ok());
}

#[test]
fn a_node_whose_address_is_not_known_does_not_say_it_is_ready() {
    let mut network = FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
    network.address = Err(StreamError::Other(
        "the interface wan has no IPv4 address".to_string(),
    ));
    let booted = boot_with(network, INPUTS, |_| {});
    assert_eq!(
        booted.line(),
        "moruna: fatal platform: the node's address is not known: the interface wan has no \
         IPv4 address"
    );
    // The address is asked before the hand-over, so api was never handed it.
    assert_eq!(booted.events.len(), 1);
    assert!(booted.api.sent.is_empty());
}

/// The OpenStack boot of 1.6d's diagnosis image (`register-late`): imagefs mounts the engine's
/// layer 10 s after it is asked, once virtio-blk has registered `system`.
fn late(layer: &mut FakeLayer) {
    layer.takes = 10 * SECOND;
}

#[test]
fn ready_waits_until_imagefs_has_mounted_the_engine_s_layer() {
    // Found by 1.6d's booted check: `ready` came 10 s before imagefs's `up` line. The node is
    // ready only once api answers and imagefs has mounted the engine's layer (1B), and boot_ms
    // counts the wait.
    let network = FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
    let booted = boot_layered(network, INPUTS, |_| {}, late);
    assert_eq!(
        booted.events,
        [
            format!("moruna: cloud openstack instance {RECORDED_OPENSTACK_ID}"),
            "the hand-over".to_string(),
            MOUNTED.to_string(),
            "moruna: ready api=10.0.2.15:443 boot_ms=10040".to_string(),
        ]
    );
    assert_eq!(booted.result.unwrap().boot_ms, Some(10_040));
    assert_eq!(booted.layer.mounted_at, Some(10 * SECOND + 40 * MS));
    // Its limit is instance data's, from platform's start, not from when it is asked.
    let network = FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
    let inputs = Inputs {
        started_ns: 3 * SECOND,
        ..INPUTS
    };
    let booted = boot_layered(network, inputs, |_| {}, late);
    assert!(booted.result.is_ok());
    assert_eq!(booted.layer.deadlines, [93 * SECOND]);
}

#[test]
fn on_azure_the_ready_report_waits_until_imagefs_has_mounted_the_engine_s_layer() {
    let clock = FakeClock::default();
    let latency = clock.clone();
    let network = FakeNetwork::serving(&clock, move |_, sent| {
        latency.advance(2 * MS);
        azure(sent)
    });
    let booted = boot_layered(network, INPUTS, |_| {}, late);
    assert!(booted.result.is_ok());
    // Without the report Azure reboots the machine; with it, Azure takes the node as ready, so
    // it is sent only once the node is: after imagefs's 10 s.
    let mounted = booted.layer.mounted_at.expect("imagefs was asked");
    assert_eq!(mounted, 10 * SECOND + 42 * MS);
    assert_eq!(
        asked(&booted),
        [
            (IMDS, "GET", AZURE_INSTANCE),
            (wireserver::ADDRESS, "GET", "/machine?comp=goalstate"),
            (wireserver::ADDRESS, "POST", "/machine?comp=health"),
        ]
    );
    assert!(booted.requests[1..].iter().all(|(at, _)| *at >= mounted));
    assert_eq!(
        booted.events.last().map(String::as_str),
        Some("moruna: ready api=10.0.2.15:443 boot_ms=10046")
    );
}

#[test]
fn an_engine_s_layer_not_mounted_is_fatal_and_names_imagefs() {
    // Whether platform's line or the supervisor's (`moruna: fatal imagefs: ended with code 1`)
    // comes first, the first fatal line names imagefs (PM, item 1.6e). Neither ready nor
    // Azure's report comes.
    let cases = [
        (
            LinkError::TimedOut,
            "moruna: fatal imagefs: did not mount the engine's layer within 90 s",
        ),
        (
            LinkError::Closed,
            "moruna: fatal imagefs: ended before it mounted the engine's layer",
        ),
        (
            LinkError::Other("the kernel refused: bad handle".to_string()),
            "moruna: fatal imagefs: did not say it mounted the engine's layer: the kernel \
             refused: bad handle",
        ),
    ];
    for (error, line) in cases {
        for on_azure in [false, true] {
            let network = FakeNetwork::serving(&FakeClock::default(), move |_, sent| {
                if on_azure {
                    azure(sent)
                } else {
                    openstack(sent)
                }
            });
            let answer = error.clone();
            let booted = boot_layered(
                network,
                INPUTS,
                |_| {},
                |layer| {
                    layer.answer = Err(answer);
                },
            );
            assert_eq!(booted.line(), line);
            assert_eq!(booted.result.as_ref().unwrap_err().stage, Stage::Imagefs);
            // api was handed its config and answered; the node never said it was ready.
            assert_eq!(booted.events.len(), 2, "{:?}", booted.events);
            assert_eq!(booted.events[1], "the hand-over");
            assert!(
                booted
                    .requests
                    .iter()
                    .all(|(_, sent)| sent.to.0 != wireserver::ADDRESS),
                "no ready report"
            );
            assert_eq!(booted.layer.deadlines, [90 * SECOND]);
            if error == LinkError::TimedOut {
                // At instance data's limit, from platform's start.
                assert_eq!(booted.clock.now(), 90 * SECOND);
            }
        }
    }
}

#[test]
fn the_lines_are_the_design_s() {
    assert_eq!(
        cloud_line(Cloud::OpenStack, "83679162-1378-4288-a2d4-70e13ec132aa"),
        "moruna: cloud openstack instance 83679162-1378-4288-a2d4-70e13ec132aa"
    );
    assert_eq!(
        cloud_line(Cloud::Azure, "5c08b38e"),
        "moruna: cloud azure instance 5c08b38e"
    );
    assert_eq!(
        cloud_line(Cloud::None, "dev"),
        "moruna: cloud none instance dev"
    );
    assert_eq!(
        ready_line([203, 0, 113, 7].into(), 2140),
        "moruna: ready api=203.0.113.7:443 boot_ms=2140"
    );
    assert_eq!(
        Fatal::new(Stage::InstanceData, "nothing by 90 s").to_string(),
        "moruna: fatal instance-data: nothing by 90 s"
    );
    assert_eq!(
        Fatal::new(Stage::Config, "a reason").to_string(),
        "moruna: fatal config: a reason"
    );
    assert_eq!(
        Fatal::new(Stage::Platform, "a reason").to_string(),
        "moruna: fatal platform: a reason"
    );
    assert_eq!(
        Fatal::new(Stage::Imagefs, "a reason").to_string(),
        "moruna: fatal imagefs: a reason"
    );
    // A reason is one printable line that fits the console's buffer, whatever it quotes.
    let quoted = format!("line\nbreak, caf\u{e9}, bell\u{7}, {}", "x".repeat(600));
    let line = Fatal::new(Stage::Config, quoted).to_string();
    assert!(line.starts_with("moruna: fatal config: line?break, caf?, bell?, xxx"));
    assert_eq!(line.len(), "moruna: fatal config: ".len() + 300);
    assert!(line.chars().all(|c| c == ' ' || c.is_ascii_graphic()));
}

#[test]
fn the_hand_over_waits_for_the_supervisor_s_channel_to_api() {
    // Each start of api, the first included, comes as a channel on the supervisor's channel
    // (PM, item 2.2c). Asked with api's 10 s, after the cloud's line.
    let network = || FakeNetwork::serving(&FakeClock::default(), |_, sent| openstack(sent));
    let cloud = format!("moruna: cloud openstack instance {RECORDED_OPENSTACK_ID}");
    let booted = boot_with(network(), INPUTS, |_| {});
    assert!(booted.result.is_ok());
    assert_eq!(booted.api.channels, [10 * SECOND]);
    for (error, said) in [
        (LinkError::TimedOut, "the deadline passed"),
        (LinkError::Closed, "the supervisor closed its channel"),
        (
            LinkError::Other("the supervisor sent something other than api-started".into()),
            "the supervisor sent something other than api-started",
        ),
    ] {
        let booted = boot_with(network(), INPUTS, |api| {
            api.starts = 0;
            api.then = error.clone();
        });
        assert_eq!(
            booted.line(),
            format!("moruna: fatal platform: the supervisor sent no channel to api: {said}")
        );
        assert_eq!(booted.result.unwrap_err().stage, Stage::Platform);
        assert_eq!(booted.events, std::slice::from_ref(&cloud));
        assert!(booted.api.sent.is_empty());
        assert_eq!(booted.api.channels, [10 * SECOND]);
    }
}

/// `serve` after a boot on OpenStack: the boot's hand-over, and `api`'s later starts as `prepare`
/// makes them, from a clock at 1000 s.
fn served(prepare: impl FnOnce(&mut FakeApi)) -> (Fatal, Handover, FakeApi) {
    let handover = boot(|_, sent| openstack(sent)).result.unwrap();
    let mut clock = FakeClock::default();
    clock.advance(1000 * SECOND);
    let events = Events::default();
    let mut api = FakeApi::new(&clock, &events);
    prepare(&mut api);
    let api = Rc::new(RefCell::new(api));
    let mut network = FakeNetwork::serving(&clock, |_, sent| openstack(sent));
    let fatal = serve(
        &mut network,
        &mut clock,
        &mut FakeWall::default(),
        &mut FakeControl(api.clone()),
        &handover,
        &mut |line: &str| panic!("serve said {line}"),
    );
    let api = Rc::try_unwrap(api).ok().unwrap().into_inner();
    (fatal, handover, api)
}

#[test]
fn each_later_start_of_api_gets_the_same_hand_over() {
    let (fatal, handover, api) = served(|api| api.starts = 3);
    // Three later starts, each handed what the first had and the node's boot_ms, the boot's
    // ready line's, each with 1D's 10 s from when its channel arrived: 40 ms apart, api's
    // answers.
    assert_eq!(api.sent, vec![handover.encode().unwrap(); 3]);
    let later = Handover::decode(&api.sent[0]).unwrap();
    assert_eq!(later.boot_ms, Some(40), "the boot's api answered 40 ms in");
    assert_eq!(later, handover);
    let t = 1000 * SECOND;
    assert_eq!(
        api.deadlines,
        [
            t + 10 * SECOND,
            t + 10 * SECOND,
            t + 40 * MS + 10 * SECOND,
            t + 40 * MS + 10 * SECOND,
            t + 80 * MS + 10 * SECOND,
            t + 80 * MS + 10 * SECOND,
        ]
    );
    // A start is waited for until the hour's re-read of the wall clock (none came here).
    assert_eq!(api.channels, [t + RETIME_NS; 4]);
    // When the supervisor's channel ends, so does platform, with the reason.
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: the supervisor's channel: the supervisor closed its channel"
    );
    assert_eq!(fatal.stage, Stage::Platform);
    // Nothing reaches the console: the supervisor's and api's own lines say what happened.
    assert_eq!(api.events.borrow().len(), 3);
    assert!(
        api.events
            .borrow()
            .iter()
            .all(|event| event == "the hand-over")
    );
}

#[test]
fn a_later_start_of_api_has_1d_s_10_s_too() {
    let (fatal, _, api) = served(|api| api.answer = Err(LinkError::TimedOut));
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: api did not answer ready within 10 s"
    );
    assert_eq!(api.deadlines, [1010 * SECOND; 2]);
    let (fatal, _, _) = served(|api| api.answer = Ok(b"READY".to_vec()));
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: api answered the hand-over with something else"
    );
    let (fatal, _, _) = served(|api| api.send = Err(LinkError::Closed));
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: the hand-over was not sent to api: api closed its end of the \
         link"
    );
    let (fatal, _, api) = served(|api| {
        api.starts = 1;
        api.then = LinkError::Other("the supervisor sent something other than api-started".into());
    });
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: the supervisor's channel: the supervisor sent something other \
         than api-started"
    );
    assert_eq!(api.sent.len(), 1);
}
