//! The engine host (system design 3A, item 3.2) over the tests' `node-admission`, the test
//! playing each engine on its end of the channel: the engine supervisor's configurations, a
//! check and a job from start to end, the host protocol in pieces of at most 64 KiB, what the
//! node's admission refuses, and a restarted `api`'s fresh connection, its predecessor's job
//! ended with it.

use std::time::{Duration, Instant};

use ::engine::{Config as EngineConfig, Source};
use moruna_api_server::{EngineHost, HostError, Needs, Verdict, Volume, check_line};
use node_protocol::{CANCELLED, check_args, serve_args};

use super::{admission::Admission, *};
use crate::engine::{Host, PIECE, Shared, Sizes, check_config, job_config, sizes};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// An 8 GiB node's job bound (the cloud profile's, item 3.2).
const BOUND: u64 = 6_580 * MIB;

fn host(admission: &Admission) -> Host {
    jobs(admission.connect()).expect("the engine host")
}

/// Every message the engine's end holds now.
fn messages(channel: &Channel) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Ok(Some((bytes, handles))) = channel.needed() {
        assert_eq!(handles, 0, "no handle in the host protocol");
        let mut message = vec![0; bytes];
        channel.read(&mut message, &mut []).expect("a message");
        out.push(message);
    }
    out
}

/// Waits, within 10 s, until `done` holds.
fn until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "not within 10 s");
        thread::sleep(Duration::from_millis(5));
    }
}

/// A sound job's verdict: a gigabyte, and a need of each kind.
fn sound() -> Verdict {
    Verdict {
        refusals: Vec::new(),
        memory_bytes: Some(GIB),
        needs: Needs {
            hosts: vec!["s3.eu-west-1.amazonaws.com".into()],
            volumes: vec![Volume {
                serial: "vol-out".into(),
                write: true,
                staging: false,
            }],
            functions: vec!["taxi.clean".into()],
        },
    }
}

const SPEC: &[u8] = b"{\"type\":\"spec\",\"spec\":{\"moruna_spec\":1}}\n";

/// A job's memory and scratch are the engine's own split of its budget, and the supervisor's
/// configurations are what its own parser takes: configure.py's engine.conf, less vsock and
/// the launcher.
#[test]
fn the_engine_s_configurations_are_its_supervisor_s() {
    assert_eq!(
        sizes(GIB, false),
        Sizes {
            memory: 672 * MIB,
            scratch: 224 * MIB
        }
    );
    // A job that stages on a volume has no in-memory scratch (item 3.3).
    assert_eq!(
        sizes(GIB, true),
        Sizes {
            memory: 896 * MIB,
            scratch: 0
        }
    );
    // Whole pages, and the parts and the engine's own make the budget.
    for budget in [GIB + 4096, 6_532 * MIB, 129 * MIB, 128 * MIB + 4096] {
        for staging in [false, true] {
            let Sizes { memory, scratch } = sizes(budget, staging);
            assert_eq!(memory % 4096, 0, "{budget}");
            assert_eq!(scratch % 4096, 0, "{budget}");
            assert_eq!(memory + scratch + 128 * MIB, budget);
        }
    }
    let staged = EngineConfig::parse(&job_config(BOUND, GIB, true).unwrap()).expect("it parses");
    assert_eq!((staged.memory, staged.scratch), (896 * MIB, 0));
    let job = EngineConfig::parse(&job_config(BOUND, GIB, false).unwrap()).expect("it parses");
    assert_eq!(job.program, "/moruna/bin/moruna");
    assert_eq!(job.args, serve_args(BOUND));
    assert_eq!(job.env, ["HOME=/tmp"]);
    assert_eq!(
        (job.memory, job.scratch, job.runs),
        (672 * MIB, 224 * MIB, 1)
    );
    let grants: Vec<(&str, &Source)> = job
        .grants
        .iter()
        .map(|g| (g.slot.as_str(), &g.source))
        .collect();
    assert_eq!(
        grants,
        [
            ("linux", &Source::Personality),
            ("network", &Source::Service("network".into())),
            ("engine", &Source::Channel("engine".into())),
        ]
    );
    // A check: 256 MiB less the engine's own, no scratch, its own channel, and the node's
    // volumes node-admission grants it (item 3.3).
    let check = EngineConfig::parse(&check_config(BOUND)).expect("it parses");
    assert_eq!(check.args, check_args(BOUND));
    assert_eq!((check.memory, check.scratch, check.runs), (128 * MIB, 0, 1));
    let grants: Vec<(&str, &Source)> = check
        .grants
        .iter()
        .skip(2)
        .map(|g| (g.slot.as_str(), &g.source))
        .collect();
    assert_eq!(
        grants,
        [
            ("check", &Source::Channel("check".into())),
            ("volumes", &Source::Data("volumes".into())),
        ]
    );
    // A budget that leaves the program nothing is refused by its field.
    for budget in [0, 64 * MIB, 128 * MIB] {
        assert!(
            matches!(job_config(BOUND, budget, false), Err(HostError::Refused(why)) if why.starts_with("budget.memory_bytes")),
            "{budget}"
        );
    }
    assert!(job_config(BOUND, 128 * MIB + 4096, false).is_ok());
}

/// A check: the host asks the bound first, starts the check with its configuration, the Linux
/// personality and the network server, writes the `check` line in pieces of at most 64 KiB,
/// and reads the `checked` line however it arrives. The next check waits for the last to end,
/// and releases it.
#[test]
fn a_check_runs_in_an_engine_of_its_own_and_answers_its_verdict() {
    let admission = Admission::start(BOUND);
    let mut host = host(&admission);
    assert_eq!(host.bound(), BOUND);
    assert!(host.waits().is_empty());
    let document = format!(
        "{{\"moruna_spec\":1,\"name\":\"{}\"}}",
        "x".repeat(150 * 1024)
    );
    let names = vec![String::from("s3.key")];
    host.check(document.as_bytes(), &names).unwrap();
    assert_eq!(host.checked(), None, "no answer yet");
    assert_eq!(host.waits().len(), 1);
    {
        let book = admission.book.lock().unwrap();
        let started = &book.started[0];
        assert_eq!(started.budget, None);
        // The check's channel, and the node's volumes, as node-admission adds them.
        assert_eq!(started.names, ["check", "volumes"]);
        assert_eq!(started.services, ["linux", "network"]);
    }
    assert_eq!(admission.config(0), check_config(BOUND));
    let engine = admission.engine(0);
    let pieces = messages(&engine);
    let sizes: Vec<usize> = pieces.iter().map(Vec::len).collect();
    let line = check_line(document.as_bytes(), &names);
    assert_eq!(sizes, [PIECE, PIECE, line.len() - 2 * PIECE]);
    assert_eq!(pieces.concat(), line);
    // The answer in two pieces.
    engine
        .write_with(b"{\"type\":\"checked\",\"ok\":true,", &mut [])
        .unwrap();
    assert_eq!(host.checked(), None);
    engine
        .write_with(b"\"memory_bytes\":1073741824}\n", &mut [])
        .unwrap();
    let verdict = host.checked().expect("the verdict").expect("sound");
    assert_eq!(verdict.memory_bytes, Some(GIB));
    assert!(verdict.refusals.is_empty());
    assert_eq!(host.checked(), None, "nothing under way");
    assert!(host.waits().is_empty());

    // The next check waits for the last one's process to end, and releases it first.
    assert_eq!(host.check(b"{}", &[]), Err(HostError::WouldBlock));
    admission.end(1);
    host.check(b"{}", &[]).unwrap();
    assert_eq!(admission.book.lock().unwrap().released, [1]);
    assert_eq!(
        host.check(b"{}", &[]),
        Err(HostError::WouldBlock),
        "one at a time"
    );
    // A check that ends without its answer fails.
    drop(admission.engine(1));
    assert!(matches!(host.checked(), Some(Err(HostError::Failed(_)))));
    // A refused document is a verdict too.
    admission.end(2);
    host.check(b"{}", &[]).unwrap();
    let engine = admission.engine(2);
    engine
        .write_with(
            b"{\"type\":\"checked\",\"ok\":false,\"refusals\":[{\"field\":\"sink\",\"message\":\"no sink\"}]}\n",
            &mut [],
        )
        .unwrap();
    let refused = host.checked().unwrap().unwrap();
    assert_eq!(refused.refusals[0].field, "sink");
}

/// A job: its engine is launched with the check's budget and needs, sent `spec` and later
/// lines, and its bytes read in whatever amounts the server asks; when its channel ends, the
/// host waits for its process and releases it before the end is reported.
#[test]
fn a_job_runs_in_a_fresh_engine_from_spec_to_its_end() {
    let admission = Admission::start(BOUND);
    let mut host = Shared(Rc::new(RefCell::new(host(&admission))));
    host.start(SPEC, &sound()).unwrap();
    {
        let book = admission.book.lock().unwrap();
        let started = &book.started[0];
        assert_eq!(started.budget, Some(GIB));
        assert_eq!(started.names, ["engine"]);
        assert_eq!(started.services, ["linux", "network"]);
        assert_eq!(
            started.needs,
            (
                vec![String::from("s3.eu-west-1.amazonaws.com")],
                vec![(String::from("vol-out"), true)],
                vec![String::from("taxi.clean")]
            )
        );
    }
    assert_eq!(admission.config(0), job_config(BOUND, GIB, false).unwrap());
    let engine = admission.engine(0);
    assert_eq!(messages(&engine).concat(), SPEC);
    assert_eq!(host.0.borrow().waits().len(), 1);

    let mut buf = [0u8; 16 * 1024];
    assert_eq!(host.receive(&mut buf), Err(HostError::WouldBlock));
    engine
        .write_with(b"{\"type\":\"hello\"}\n", &mut [])
        .unwrap();
    let big = vec![b'y'; PIECE];
    engine.write_with(&big, &mut []).unwrap();
    let mut got = Vec::new();
    loop {
        match host.receive(&mut buf) {
            Ok(count) => {
                assert!(count > 0);
                got.extend_from_slice(&buf[..count]);
            }
            Err(HostError::WouldBlock) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert_eq!(&got[..17], b"{\"type\":\"hello\"}\n");
    assert_eq!(got.len(), 17 + PIECE);
    host.send(b"{\"type\":\"cancel\"}\n").unwrap();
    assert_eq!(messages(&engine).concat(), b"{\"type\":\"cancel\"}\n");
    // A second job while one runs is not the node's to start.
    assert_eq!(host.start(SPEC, &sound()), Err(HostError::Unavailable));

    // The engine ends, then its channel closes.
    admission.end(1);
    drop(engine);
    assert_eq!(host.receive(&mut buf), Ok(0));
    {
        let book = admission.book.lock().unwrap();
        assert_eq!(
            (book.waited.as_slice(), book.released.as_slice()),
            (&[1][..], &[1][..])
        );
    }
    assert!(host.0.borrow().waits().is_empty());
    assert_eq!(host.receive(&mut buf), Err(HostError::WouldBlock));
    assert_eq!(host.send(b"{}\n"), Err(HostError::Unavailable));
    // The account is free again: the next job starts.
    host.start(SPEC, &sound()).unwrap();
    // And a check beside it.
    host.check(b"{}", &[]).unwrap();
    assert_eq!(host.checked(), None);
}

/// What the node's admission refuses, and what no engine can be started for.
#[test]
fn what_admission_refuses_is_the_call_s_answer() {
    let admission = Admission::start(BOUND);
    let mut host = host(&admission);
    let none = Verdict {
        memory_bytes: None,
        ..sound()
    };
    assert!(matches!(host.start(SPEC, &none), Err(HostError::Failed(_))));
    let small = Verdict {
        memory_bytes: Some(64 * MIB),
        ..sound()
    };
    assert!(matches!(
        host.start(SPEC, &small),
        Err(HostError::Refused(_))
    ));
    // Past the node's bound: admission's `Quota`.
    let large = Verdict {
        memory_bytes: Some(BOUND + 4096),
        ..sound()
    };
    assert!(
        matches!(host.start(SPEC, &large), Err(HostError::Refused(why)) if why.starts_with("budget.memory_bytes"))
    );
    admission.book.lock().unwrap().refuse = Some(node_protocol::admission::Error::Unavailable);
    assert!(matches!(host.check(b"{}", &[]), Err(HostError::Failed(_))));
    admission.book.lock().unwrap().refuse = Some(node_protocol::admission::Error::Limit);
    assert_eq!(host.check(b"{}", &[]), Err(HostError::WouldBlock));
    assert!(admission.book.lock().unwrap().started.is_empty());
    // A node-admission that has gone.
    let (ours, theirs) = Channel::create().unwrap();
    drop(theirs);
    assert!(matches!(jobs(ours), Err(Error::Admission("hello", _))));
}

/// A restarted `api` (item 3.2, with 2.2c's restart): each start gets a fresh connection
/// through the supervisor's link, and a job the dead one started is ended with its connection.
#[test]
fn a_restarted_api_has_a_fresh_connection_and_its_predecessor_s_job_ended() {
    let admission = Admission::start(BOUND);
    let mut first = host(&admission);
    first.start(SPEC, &sound()).unwrap();
    let engine = admission.engine(0);
    // `api` dies: its connection closes with it.
    drop(first);
    until(|| admission.book.lock().unwrap().closed.len() == 1);
    assert_eq!(
        admission.book.lock().unwrap().states.get(&1),
        Some(&CANCELLED)
    );
    drop(engine);
    // The next start's connection works.
    let mut second = host(&admission);
    assert_eq!(second.bound(), BOUND);
    second.check(b"{}", &[]).unwrap();
    let book = admission.book.lock().unwrap();
    assert_eq!(book.started.len(), 2);
    assert_ne!(book.started[0].owner, book.started[1].owner);
    assert_eq!(
        Error::Admission("hello", CallError::PeerClosed).to_string(),
        "node-admission: hello: PeerClosed"
    );
}
