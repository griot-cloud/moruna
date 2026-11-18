//! The node's one wall clock (system design 3D): `platform` sets it from the `Date` of the answer
//! that named the cloud before the node is ready, and again every hour from its cloud's instance
//! document.

use std::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use moruna_node_config::Cloud;

use super::{
    FakeClock, FakeNetwork, FakeWall, RECORDED_OPENSTACK_TIME, Reply, SECOND, Sent, answer, azure,
    openstack,
};
use crate::{
    boot::{Inputs, REFRESHED, RETIME_LIMIT_NS, RETIME_NS, Refresh, retime, run, serve},
    machine::{Clock, Control, Link, LinkError},
};

const META: &str = "/openstack/latest/meta_data.json";
const USER: &str = "/openstack/latest/user_data";

/// `api`, answering ready at once.
struct Ready;

impl Link for Ready {
    fn send(&mut self, _: &[u8], _: u64) -> Result<(), LinkError> {
        Ok(())
    }

    fn receive(&mut self, _: u64) -> Result<Vec<u8>, LinkError> {
        Ok(b"ready".to_vec())
    }
}

/// The supervisor's channel: each wait for a start of `api` meets the next of `waits`, a start
/// (`Ok`) or an error; one that times out first takes the clock to its deadline. After the last,
/// the channel is closed.
struct Starts {
    clock: FakeClock,
    waits: Vec<Result<(), LinkError>>,
    /// The deadline of each wait.
    deadlines: Vec<u64>,
}

impl Control for Starts {
    type Api = Ready;

    fn next_api(&mut self, deadline: u64) -> Result<Ready, LinkError> {
        self.deadlines.push(deadline);
        if self.waits.is_empty() {
            return Err(LinkError::Closed);
        }
        match self.waits.remove(0) {
            Ok(()) => Ok(Ready),
            Err(LinkError::TimedOut) => {
                self.clock.sleep_until(deadline);
                Err(LinkError::TimedOut)
            }
            Err(error) => Err(error),
        }
    }
}

/// A channel on which the boot's one start of `api` comes at once.
fn one_start(clock: &FakeClock) -> Starts {
    Starts {
        clock: clock.clone(),
        waits: vec![Ok(())],
        deadlines: Vec::new(),
    }
}

const INPUTS: Inputs = Inputs {
    started_ns: 0,
    memory_bytes: 8 << 30,
};

#[test]
fn the_clock_reads_the_date_as_of_when_its_answer_arrived() {
    // OpenStack's user-data is asked again a second later: the clock is set then, to the Date of
    // the instance document plus that second.
    let mut asked_user_data = 0;
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, move |_, sent| match sent.path.as_str() {
        USER if asked_user_data == 0 => {
            asked_user_data += 1;
            Reply::Answer(answer("503 Service Unavailable", b""))
        }
        _ => openstack(sent),
    });
    let mut wall = FakeWall::default();
    let mut lines = Vec::new();
    let handover = run(
        &mut network,
        &mut clock.clone(),
        &mut wall,
        &mut one_start(&clock),
        &mut |line: &str| lines.push(line.to_string()),
        INPUTS,
    )
    .unwrap();
    assert_eq!(handover.wall_clock, RECORDED_OPENSTACK_TIME);
    assert_eq!(handover.read_at_ns, 0);
    assert_eq!(clock.now(), SECOND);
    assert_eq!(wall.times(), [(RECORDED_OPENSTACK_TIME + 1) * SECOND]);
    assert_eq!(lines.len(), 2);
}

#[test]
fn a_time_the_core_refuses_is_fatal_before_the_cloud_line() {
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| openstack(sent));
    let mut wall = FakeWall {
        refuses: Some("InvalidArgs".into()),
        ..FakeWall::default()
    };
    let mut lines = Vec::new();
    let fatal = run(
        &mut network,
        &mut clock.clone(),
        &mut wall,
        &mut one_start(&clock),
        &mut |line: &str| lines.push(line.to_string()),
        INPUTS,
    )
    .unwrap_err();
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal instance-data: the wall clock was not set from the Date header: InvalidArgs"
    );
    // Nothing was said or handed over: the clock comes before the config and the ready line.
    assert!(lines.is_empty());
    assert!(wall.times().is_empty());
}

#[test]
fn every_hour_the_clock_is_set_again_from_the_cloud_s_instance_document() {
    assert_eq!(RETIME_NS, 3600 * SECOND);
    // Azure: its instance document, with `Metadata: true`.
    let clock = FakeClock::default();
    clock.advance(RETIME_NS);
    let mut network = FakeNetwork::serving(&clock, |_, sent| azure(sent));
    let mut wall = FakeWall::default();
    let set = retime(&mut network, &mut clock.clone(), &mut wall, Cloud::Azure).unwrap();
    assert_eq!(set, RECORDED_OPENSTACK_TIME + 2);
    assert_eq!(wall.times(), [set * SECOND]);
    assert_eq!(
        network.paths(),
        ["/metadata/instance?api-version=2021-02-01"]
    );
    assert_eq!(network.requests[0].1.header("Metadata"), Some("true"));
    // OpenStack: its instance document, the Date as of when its answer arrived.
    let clock = FakeClock::default();
    let latency = clock.clone();
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        latency.advance(3 * SECOND);
        openstack(sent)
    });
    let mut wall = FakeWall::default();
    let set = retime(
        &mut network,
        &mut clock.clone(),
        &mut wall,
        Cloud::OpenStack,
    )
    .unwrap();
    assert_eq!(set, RECORDED_OPENSTACK_TIME);
    assert_eq!(wall.times(), [RECORDED_OPENSTACK_TIME * SECOND]);
    assert_eq!(network.paths(), [META]);
}

/// What the fake instance data does with a request.
type Serve = Box<dyn FnMut(u64, &Sent) -> Reply>;

/// One re-read against `serve`: what it returned, the times the clock was set to, and how long
/// it took.
fn retimed(
    cloud: Cloud,
    wall: &mut FakeWall,
    serve: impl FnMut(u64, &Sent) -> Reply + 'static,
) -> (Result<u64, String>, Vec<u64>, u64) {
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, serve);
    let result = retime(&mut network, &mut clock.clone(), wall, cloud);
    (result, wall.times(), clock.now())
}

#[test]
fn a_re_read_that_fails_leaves_the_clock_as_it_was() {
    let no_date = |_: u64, _: &Sent| {
        Reply::Answer(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec())
    };
    let bad_date = |_: u64, _: &Sent| {
        Reply::Answer(
            b"HTTP/1.1 200 OK\r\nDate: Sunday, 06-Nov-94 08:49:37 GMT\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        )
    };
    let cases: Vec<(Cloud, Serve, String)> = vec![
        (
            Cloud::OpenStack,
            Box::new(|_, _| Reply::Answer(answer("503 Service Unavailable", b""))),
            format!("GET {META}: answered 503"),
        ),
        (
            Cloud::OpenStack,
            Box::new(no_date),
            format!("GET {META}: its answer has no single Date header"),
        ),
        (
            Cloud::Azure,
            Box::new(bad_date),
            "the Date header is not an IMF-fixdate".to_string(),
        ),
        (
            Cloud::None,
            Box::new(|_, sent| openstack(sent)),
            "a node with no cloud has no instance data".to_string(),
        ),
    ];
    for (cloud, serve, reason) in cases {
        let (result, times, _) = retimed(cloud, &mut FakeWall::default(), serve);
        assert_eq!(result, Err(reason));
        assert!(times.is_empty());
    }
    // A server that does not answer: given up at the limit, the clock untouched.
    let (result, times, took) = retimed(Cloud::OpenStack, &mut FakeWall::default(), |_, _| {
        Reply::Stalls
    });
    assert!(result.unwrap_err().starts_with(&format!("GET {META}: ")));
    assert!(times.is_empty());
    assert_eq!(took, RETIME_LIMIT_NS);
    // The core refuses the time.
    let mut refusing = FakeWall {
        refuses: Some("InvalidArgs".into()),
        ..FakeWall::default()
    };
    let (result, times, _) = retimed(Cloud::OpenStack, &mut refusing, |_, sent| openstack(sent));
    assert_eq!(result, Err("InvalidArgs".to_string()));
    assert!(times.is_empty());
}

#[test]
fn a_failed_re_read_is_said_once_and_its_recovery_once() {
    // The instance document fails, fails, answers, answers, fails, fails, answers, hour by hour:
    // each failure after an answer is said once, and each answer after a failure.
    let failing = [true, true, false, false, true, true, false];
    let mut hour = 0;
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        let fails = failing[hour];
        hour += 1;
        if fails {
            Reply::Answer(answer("503 Service Unavailable", b"\x07"))
        } else {
            openstack(sent)
        }
    });
    let mut wall = FakeWall::default();
    let mut refresh = Refresh::default();
    let mut lines = Vec::new();
    for _ in 0..7 {
        refresh.run(
            &mut network,
            &mut clock.clone(),
            &mut wall,
            Cloud::OpenStack,
            &mut |line: &str| lines.push(line.to_string()),
        );
    }
    let not_refreshed = format!("platform: wall clock not refreshed: GET {META}: answered 503");
    assert_eq!(
        lines,
        [
            not_refreshed.clone(),
            REFRESHED.to_string(),
            not_refreshed,
            REFRESHED.to_string()
        ]
    );
    // The clock was set by each re-read that answered, and by no other.
    assert_eq!(wall.times(), [RECORDED_OPENSTACK_TIME * SECOND; 3]);
    // A reason from outside the node is shown as printable ASCII.
    assert_eq!(
        crate::boot::not_refreshed_line("a\tb\u{e9}"),
        "platform: wall clock not refreshed: a?b?"
    );
}

#[test]
fn between_starts_of_api_the_clock_is_re_read_every_hour() {
    // After the boot: no start of api for two hours, then one, then an hour of nothing; the
    // instance document answers the first re-read, fails the second, answers the third.
    let clock = FakeClock::default();
    clock.advance(1000 * SECOND);
    let mut asked = 0;
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        asked += 1;
        if asked == 2 {
            Reply::Answer(answer("503 Service Unavailable", b""))
        } else {
            openstack(sent)
        }
    });
    let mut control = Starts {
        clock: clock.clone(),
        waits: vec![
            Err(LinkError::TimedOut),
            Err(LinkError::TimedOut),
            Ok(()),
            Err(LinkError::TimedOut),
        ],
        deadlines: Vec::new(),
    };
    let handover = moruna_node_config::Handover {
        config: moruna_node_config::NodeConfig::validate(
            super::RECORDED_CONFIG.as_bytes(),
            8 << 30,
        )
        .unwrap(),
        instance: super::RECORDED_OPENSTACK_ID.into(),
        cloud: Cloud::OpenStack,
        wall_clock: RECORDED_OPENSTACK_TIME,
        read_at_ns: 0,
        memory_bytes: 8 << 30,
        boot_ms: Some(46),
    };
    let mut wall = FakeWall::default();
    let mut lines = Vec::new();
    let fatal = serve(
        &mut network,
        &mut clock.clone(),
        &mut wall,
        &mut control,
        &handover,
        &mut |line: &str| lines.push(line.to_string()),
    );
    // Each wait lasts until the hour's re-read; the start of api does not move the hour.
    let hour = 1000 * SECOND + RETIME_NS;
    assert_eq!(
        control.deadlines,
        [
            hour,
            hour + RETIME_NS,
            hour + 2 * RETIME_NS,
            hour + 2 * RETIME_NS,
            hour + 3 * RETIME_NS
        ]
    );
    // The first re-read set the clock; the second failed and said so; the third set it again
    // and said that too.
    assert_eq!(
        lines,
        [
            format!("platform: wall clock not refreshed: GET {META}: answered 503"),
            REFRESHED.to_string()
        ]
    );
    assert_eq!(wall.times(), [RECORDED_OPENSTACK_TIME * SECOND; 2]);
    // When the supervisor's channel ends, so does platform, with the reason.
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: the supervisor's channel: the supervisor closed its channel"
    );
}
