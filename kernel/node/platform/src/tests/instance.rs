//! Finding the cloud: detection over the recorded answers, and the retry, pace and deadline rules
//! on the tests' clock.

use std::{
    cell::Cell,
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use moruna_node_config::{Cloud, Refusal};

use super::{
    FakeClock, FakeNetwork, RECORDED_AZURE_ID, RECORDED_CONFIG, RECORDED_OPENSTACK_ID, Reply,
    SECOND, Sent, answer, azure, openstack, recorded,
};
use crate::{
    boot::{Fatal, Stage},
    instance::{self, Found, find},
    machine::{Clock, StreamError},
};

const AZURE: &str = "/metadata/instance?api-version=2021-02-01";
const META: &str = "/openstack/latest/meta_data.json";
const USER: &str = "/openstack/latest/user_data";
const MS: u64 = 1_000_000;

/// The fatal line of a search that found nothing.
fn fatal(result: Result<Found, Fatal>) -> String {
    let fatal = result.expect_err("no cloud is found");
    assert_eq!(fatal.stage, Stage::InstanceData);
    fatal.to_string()
}

/// A server that takes `latency` to answer, then answers as `serve` does.
fn slow(
    clock: &FakeClock,
    latency: u64,
    mut serve: impl FnMut(&Sent) -> Reply + 'static,
) -> impl FnMut(u64, &Sent) -> Reply + 'static {
    let clock = clock.clone();
    move |_, sent| {
        clock.advance(latency);
        serve(sent)
    }
}

#[test]
fn openstack_is_found_from_its_recorded_answers() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, slow(&clock, 2 * MS, openstack));
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(
        found,
        Found {
            cloud: Cloud::OpenStack,
            instance: RECORDED_OPENSTACK_ID.to_string(),
            date: Some("Tue, 17 Nov 2026 09:30:00 GMT".to_string()),
            // When the document's answer had arrived: after Azure's path and its own.
            read_at_ns: 4 * MS,
            user_data: Ok(RECORDED_CONFIG.as_bytes().to_vec()),
        }
    );
    // Azure's path first, which OpenStack answers 404; then the document; then the user-data.
    assert_eq!(network.paths(), [AZURE, META, USER]);
    assert_eq!(network.attempts, [0, 2 * MS, 4 * MS]);
    for (_, sent) in &network.requests {
        assert_eq!(sent.to, (instance::ADDRESS, 80));
        assert_eq!(sent.method, "GET");
        assert_eq!(sent.header("Host"), Some("169.254.169.254"));
        assert_eq!(sent.header("Connection"), Some("close"));
        assert!(sent.body.is_empty());
        // Only Azure's request carries Azure's header.
        assert_eq!(
            sent.header("Metadata"),
            (sent.path == AZURE).then_some("true")
        );
    }
}

#[test]
fn azure_is_found_from_its_recorded_answer() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, slow(&clock, 3 * MS, azure));
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(
        found,
        Found {
            cloud: Cloud::Azure,
            instance: RECORDED_AZURE_ID.to_string(),
            date: Some("Tue, 17 Nov 2026 09:30:02 GMT".to_string()),
            read_at_ns: 3 * MS,
            // `compute.userData`, out of its base64.
            user_data: Ok(RECORDED_CONFIG.as_bytes().to_vec()),
        }
    );
    assert_eq!(network.paths(), [AZURE]);
    // What is printed of it says how much user-data there is, and not what it holds.
    let shown = format!("{found:?}");
    assert!(shown.contains("user_data: Ok(82)"), "{shown}");
    assert!(shown.contains(RECORDED_AZURE_ID));
}

#[test]
fn azure_s_400_and_404_to_openstack_s_paths_move_on_to_azure_again() {
    // Azure answers 400 to a request without `Metadata: true`, which OpenStack's are, and 404 to
    // a path it does not have. Either way OpenStack is not named, and Azure is asked again.
    for (to_openstack, status) in [
        (recorded::azure_no_header as fn() -> Vec<u8>, 400),
        (recorded::azure_unknown_path as fn() -> Vec<u8>, 404),
    ] {
        let mut clock = FakeClock::default();
        let asked = Rc::new(Cell::new(0));
        let count = asked.clone();
        let mut network = FakeNetwork::serving(&clock, move |_, sent| {
            count.set(count.get() + 1);
            match (count.get(), sent.path.as_str()) {
                // Azure is busy the first time it is asked.
                (1, AZURE) => Reply::Answer(answer("503 Service Unavailable", b"")),
                (_, META) => Reply::Answer(to_openstack()),
                _ => azure(sent),
            }
        });
        let found = find(&mut network, &mut clock, 0).unwrap();
        assert_eq!(found.cloud, Cloud::Azure, "after a {status}");
        assert_eq!(found.instance, RECORDED_AZURE_ID);
        assert_eq!(network.paths(), [AZURE, META, AZURE]);
        // Other answers are followed at once.
        assert_eq!(network.attempts, [0, 0, 0]);
        assert_eq!(asked.get(), 3);
    }
    // The real service's 400, when OpenStack's path arrives without the header.
    let mut clock = FakeClock::default();
    let first = Rc::new(Cell::new(true));
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        if first.replace(false) {
            Reply::Answer(answer("429 Too Many Requests", b"{}"))
        } else {
            azure(sent)
        }
    });
    assert_eq!(
        find(&mut network, &mut clock, 0).unwrap().cloud,
        Cloud::Azure
    );
    assert_eq!(network.paths(), [AZURE, META, AZURE]);
}

#[test]
fn a_200_names_a_cloud_only_when_its_body_is_that_cloud_s_document() {
    // A server that answers every path with OpenStack's document: Azure's path gets a 200, and
    // it is not Azure's document, so Azure is not named.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| match sent.path.as_str() {
        USER => openstack(sent),
        _ => Reply::Answer(recorded::openstack_meta_data()),
    });
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(found.cloud, Cloud::OpenStack);
    assert_eq!(network.paths(), [AZURE, META, USER]);

    // Bodies that are not a cloud's document never name it, whatever the status.
    let azure_like = [
        "",
        "not json",
        "[]",
        "\"compute\"",
        "{}",
        r#"{"compute":[]}"#,
        r#"{"compute":{}}"#,
        r#"{"compute":{"vmId":7}}"#,
        r#"{"compute":{"vmId":""}}"#,
        r#"{"compute":{"vmId":"two words"}}"#,
        r#"{"compute":{"vmId":"5c08b38e","userData":5}}"#,
        r#"{"vmId":"5c08b38e-4d57-4c23-ac45-aca61037f084"}"#,
    ];
    let openstack_like = [
        "",
        "not json",
        "[]",
        "{}",
        r#"{"uuid":7}"#,
        r#"{"uuid":""}"#,
        r#"{"uuid":"line\nbreak"}"#,
        r#"{"name":"83679162-1378-4288-a2d4-70e13ec132aa"}"#,
    ];
    for (path, bodies, cloud) in [
        (AZURE, &azure_like[..], "Azure"),
        (META, &openstack_like[..], "OpenStack"),
    ] {
        for body in bodies {
            let mut clock = FakeClock::default();
            let body = body.as_bytes().to_vec();
            let mut network = FakeNetwork::serving(&clock, move |_, sent| {
                Reply::Answer(if sent.path == path {
                    answer("200 OK", &body)
                } else {
                    answer("404 Not Found", b"")
                })
            });
            let line = fatal(find(&mut network, &mut clock, 0));
            assert!(!network.paths().contains(&USER));
            // The search ends on OpenStack's path, the 450th request.
            let said = if path == META {
                format!("answered 200, and its body is not {cloud}'s document")
            } else {
                "answered 404".to_string()
            };
            assert_eq!(
                line,
                format!("moruna: fatal instance-data: nothing by 90 s: GET {META}: {said}")
            );
        }
    }
}

#[test]
fn azure_s_user_data_is_base64_or_there_is_none() {
    let with = |user_data: &str| {
        let mut clock = FakeClock::default();
        let body = format!(r#"{{"compute":{{"vmId":"{RECORDED_AZURE_ID}"{user_data}}}}}"#);
        let mut network = FakeNetwork::serving(&clock, move |_, _| {
            Reply::Answer(answer("200 OK", body.as_bytes()))
        });
        let found = find(&mut network, &mut clock, 0).unwrap();
        assert_eq!(found.cloud, Cloud::Azure);
        found.user_data
    };
    assert_eq!(with(""), Err(Refusal::Missing));
    assert_eq!(with(r#","userData":null"#), Err(Refusal::Missing));
    assert_eq!(with(r#","userData":"""#), Err(Refusal::Missing));
    assert_eq!(with(r#","userData":"e30=""#), Ok(b"{}".to_vec()));
    for text in ["{}", "e30", "e30=\\n", "e30-"] {
        let field = format!(r#","userData":"{text}""#);
        assert_eq!(with(&field), Err(Refusal::NotBase64), "{text:?}");
    }
}

#[test]
fn a_refused_connection_is_tried_again_every_second() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| openstack(sent)).connecting(|now| {
        if now < 4 * SECOND + SECOND / 2 {
            Err((StreamError::Refused, 0))
        } else {
            Ok(())
        }
    });
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(found.cloud, Cloud::OpenStack);
    // One attempt a second while nothing listens, then the three requests at once.
    let seconds: Vec<u64> = network.attempts.iter().map(|at| at / SECOND).collect();
    assert_eq!(seconds, [0, 1, 2, 3, 4, 5, 5, 5]);
    assert!(network.attempts.iter().all(|at| at % SECOND == 0));
    assert_eq!(network.paths(), [AZURE, META, USER]);
    assert_eq!(found.read_at_ns, 5 * SECOND);
}

#[test]
fn a_request_that_cannot_begin_for_lack_of_an_address_begins_as_soon_as_it_is_bound() {
    // Work item 1.11f, boot 1875 on Azure: the first request began 8 ms before the lease, had
    // no route, and the next began a whole second after it. For a lease early and late: the
    // request that could not begin is made again when the address is bound, not a second after
    // it began.
    for (lease, fails_in) in [(8 * MS, 0), (2 * SECOND + 500 * MS, 10 * MS)] {
        let mut clock = FakeClock::default();
        let mut network =
            FakeNetwork::serving(&clock, |_, sent| azure(sent)).connecting(move |now| {
                if now < lease {
                    Err((StreamError::Unreachable, fails_in))
                } else {
                    Ok(())
                }
            });
        network.address_at = lease;
        let found = find(&mut network, &mut clock, 0).unwrap();
        assert_eq!(found.cloud, Cloud::Azure);
        assert_eq!(network.attempts, [0, lease]);
        // The address was waited for once, within instance data's limit.
        assert_eq!(network.address_asked, [(fails_in, instance::LIMIT_NS)]);
        assert_eq!(found.read_at_ns, lease);
    }
}

#[test]
fn with_an_address_a_request_with_no_route_is_asked_again_a_second_later() {
    // The node has its address, but no route to instance data until 2 s: the address is asked
    // once, answered at once, and from then on the request waits out the second as a refused
    // one does.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| azure(sent)).connecting(|now| {
        if now < 2 * SECOND {
            Err((StreamError::Unreachable, 0))
        } else {
            Ok(())
        }
    });
    assert_eq!(
        find(&mut network, &mut clock, 0).unwrap().cloud,
        Cloud::Azure
    );
    assert_eq!(network.attempts, [0, 0, SECOND, 2 * SECOND]);
    assert_eq!(network.address_asked, [(0, instance::LIMIT_NS)]);
}

#[test]
fn an_address_that_cannot_be_had_leaves_the_request_to_the_second() {
    // The network server cannot say the address: the request is asked again a second after it
    // began, and the address with it.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| azure(sent)).connecting(|now| {
        if now < 2 * SECOND {
            Err((StreamError::Unreachable, 10 * MS))
        } else {
            Ok(())
        }
    });
    network.address = Err(StreamError::Other("the network server has gone".into()));
    assert_eq!(
        find(&mut network, &mut clock, 0).unwrap().cloud,
        Cloud::Azure
    );
    assert_eq!(network.attempts, [0, SECOND, 2 * SECOND]);
    assert_eq!(network.address_asked.len(), 2);
}

#[test]
fn no_address_by_90_s_is_fatal_and_says_so() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| panic!("no route"))
        .connecting(|_| Err((StreamError::Unreachable, 0)));
    network.address_at = u64::MAX;
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(
        line,
        format!(
            "moruna: fatal instance-data: nothing by 90 s: GET {AZURE}: no route to the address, \
             and the node's address: the deadline passed"
        )
    );
    // One request, then the wait for the address, to the limit.
    assert_eq!(network.attempts, [0]);
    assert_eq!(network.address_asked, [(0, instance::LIMIT_NS)]);
    assert_eq!(clock.now(), instance::LIMIT_NS);
}

#[test]
fn nothing_by_90_s_is_fatal() {
    // No server at all: every connection is refused.
    for started in [0, 7 * SECOND] {
        let mut clock = FakeClock::default();
        clock.sleep_until(started);
        let mut network = FakeNetwork::serving(&clock, |_, _| panic!("no server"))
            .connecting(|_| Err((StreamError::Refused, 0)));
        let line = fatal(find(&mut network, &mut clock, started));
        assert_eq!(
            line,
            format!(
                "moruna: fatal instance-data: nothing by 90 s: GET {AZURE}: connection refused"
            )
        );
        // Ninety attempts, a second apart, and the verdict when the limit passes: not before.
        assert_eq!(network.attempts.len(), 90);
        assert_eq!(network.attempts[0], started);
        assert_eq!(network.attempts[89], started + 89 * SECOND);
        assert_eq!(clock.now(), started + 90 * SECOND);
    }
}

#[test]
fn a_connection_that_times_out_is_tried_again_until_the_limit() {
    // The network server gives up on a connection after 30 s.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| panic!("no server"))
        .connecting(|_| Err((StreamError::TimedOut, 30 * SECOND)));
    let line = fatal(find(&mut network, &mut clock, 0));
    // More than a second has passed when each fails, so the next begins at once; the third is
    // cut by the limit.
    assert_eq!(network.attempts, [0, 30 * SECOND, 60 * SECOND]);
    assert_eq!(clock.now(), 90 * SECOND);
    assert_eq!(
        line,
        format!("moruna: fatal instance-data: nothing by 90 s: GET {AZURE}: the deadline passed")
    );
    // One that times out sooner is seen as that.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| panic!("no server"))
        .connecting(|_| Err((StreamError::TimedOut, 20 * SECOND)));
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(network.attempts.len(), 5);
    assert!(line.ends_with(": the deadline passed"), "{line}");
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| panic!("no server"))
        .connecting(|_| Err((StreamError::TimedOut, 40 * SECOND)));
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(network.attempts, [0, 40 * SECOND, 80 * SECOND]);
    assert!(line.ends_with(": the deadline passed"), "{line}");
}

#[test]
fn a_server_that_never_answers_is_cut_at_the_limit() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| Reply::Stalls);
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(network.paths(), [AZURE]);
    assert_eq!(clock.now(), 90 * SECOND);
    assert_eq!(
        line,
        format!("moruna: fatal instance-data: nothing by 90 s: GET {AZURE}: the deadline passed")
    );
}

#[test]
fn a_410_is_asked_again_a_second_later() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |now, sent| {
        if now < 2 * SECOND + SECOND / 2 {
            Reply::Answer(answer("410 Gone", b"{ \"error\": \"Gone\" }"))
        } else {
            azure(sent)
        }
    });
    assert_eq!(
        find(&mut network, &mut clock, 0).unwrap().cloud,
        Cloud::Azure
    );
    // The same cloud each time: a 410 is not another answer.
    assert_eq!(network.paths(), [AZURE, AZURE, AZURE, AZURE]);
    assert_eq!(network.attempts, [0, SECOND, 2 * SECOND, 3 * SECOND]);

    // And for ever, until the limit.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| Reply::Answer(answer("410 Gone", b"")));
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(network.attempts.len(), 90);
    assert!(network.paths().iter().all(|path| *path == AZURE));
    assert_eq!(
        line,
        format!("moruna: fatal instance-data: nothing by 90 s: GET {AZURE}: answered 410")
    );
}

#[test]
fn other_answers_move_on_at_once_and_never_six_requests_begin_in_a_second() {
    // A server that answers 404 to everything, at once.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| {
        Reply::Answer(recorded::openstack_azure_path())
    });
    let line = fatal(find(&mut network, &mut clock, 0));
    // Five requests a second for 90 s, the two clouds in turn.
    assert_eq!(network.attempts.len(), 450);
    for (index, at) in network.attempts.iter().enumerate() {
        assert_eq!(*at, (index as u64 / 5) * SECOND, "request {index}");
    }
    for (index, path) in network.paths().iter().enumerate() {
        assert_eq!(*path, if index % 2 == 0 { AZURE } else { META });
    }
    assert_eq!(clock.now(), 90 * SECOND);
    assert_eq!(
        line,
        format!("moruna: fatal instance-data: nothing by 90 s: GET {META}: answered 404")
    );

    // Whatever each request takes, the sixth never begins within a second of the first.
    for latency in [
        MS,
        50 * MS,
        100 * MS,
        199 * MS,
        200 * MS,
        300 * MS,
        999 * MS,
        1500 * MS,
    ] {
        let mut clock = FakeClock::default();
        let mut network = FakeNetwork::serving(
            &clock,
            slow(&clock, latency, |_| {
                Reply::Answer(answer("404 Not Found", b""))
            }),
        );
        fatal(find(&mut network, &mut clock, 0));
        let attempts = &network.attempts;
        for index in 5..attempts.len() {
            assert!(
                attempts[index] - attempts[index - 5] >= SECOND,
                "request {index} at a latency of {latency} ns"
            );
        }
        // And the pace adds no wait a slow server has already made.
        if latency >= 200 * MS {
            for (index, at) in attempts.iter().enumerate() {
                assert_eq!(*at, index as u64 * latency);
            }
        } else {
            assert_eq!(attempts.len(), 450);
        }
        assert!(attempts.iter().all(|at| *at < 90 * SECOND));
    }
}

#[test]
fn an_instance_created_without_user_data_has_none() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| match sent.path.as_str() {
        USER => Reply::Answer(recorded::openstack_no_user_data()),
        _ => openstack(sent),
    });
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(found.cloud, Cloud::OpenStack);
    assert_eq!(found.instance, RECORDED_OPENSTACK_ID);
    assert_eq!(found.user_data, Err(Refusal::Missing));
    assert_eq!(network.paths(), [AZURE, META, USER]);
}

#[test]
fn once_openstack_is_named_only_its_user_data_is_asked() {
    // The user-data's answers, in turn: none of the first four is the user-data or its absence.
    let mut clock = FakeClock::default();
    let turn = Rc::new(Cell::new(0));
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        if sent.path != USER {
            return openstack(sent);
        }
        turn.set(turn.get() + 1);
        match turn.get() {
            1 => Reply::Answer(answer("503 Service Unavailable", b"")),
            2 => Reply::Answer(answer("410 Gone", b"")),
            3 => Reply::Reset(b"HTTP/1.1 200 OK\r\nContent-Le".to_vec()),
            4 => Reply::Answer(b"HTTP/1.0 200 OK\r\n\r\n".to_vec()),
            _ => openstack(sent),
        }
    });
    let found = find(&mut network, &mut clock, 0).unwrap();
    assert_eq!(found.cloud, Cloud::OpenStack);
    assert_eq!(found.user_data, Ok(RECORDED_CONFIG.as_bytes().to_vec()));
    assert_eq!(network.paths(), [AZURE, META, USER, USER, USER, USER, USER]);
    // Each a second after the one before.
    let seconds: Vec<u64> = network.attempts.iter().map(|at| at / SECOND).collect();
    assert_eq!(seconds, [0, 0, 0, 1, 2, 3, 4]);
    // The clock is the document's, not the user-data's.
    assert_eq!(found.read_at_ns, 0);
    assert_eq!(found.date.as_deref(), Some("Tue, 17 Nov 2026 09:30:00 GMT"));

    // User-data that never comes is fatal at the limit, like the rest of instance data.
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| match sent.path.as_str() {
        USER => Reply::Answer(answer("500 Internal Server Error", b"")),
        _ => openstack(sent),
    });
    let line = fatal(find(&mut network, &mut clock, 0));
    assert_eq!(network.attempts.len(), 92);
    assert_eq!(clock.now(), 90 * SECOND);
    assert_eq!(
        line,
        format!("moruna: fatal instance-data: nothing by 90 s: GET {USER}: answered 500")
    );
}

#[test]
fn an_answer_the_client_does_not_read_is_another_answer_and_a_broken_one_is_none() {
    // Too large, or not HTTP/1.1: the cloud is not named, and the next is asked at once.
    let unread = [
        b"HTTP/1.1 200 OK\r\nContent-Length: 2000000\r\n\r\n".to_vec(),
        b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
    ];
    for bytes in unread {
        let mut clock = FakeClock::default();
        let mut network = FakeNetwork::serving(&clock, move |_, sent| match sent.path.as_str() {
            AZURE => Reply::Answer(bytes.clone()),
            _ => openstack(sent),
        });
        assert_eq!(
            find(&mut network, &mut clock, 0).unwrap().cloud,
            Cloud::OpenStack
        );
        assert_eq!(network.attempts, [0, 0, 0]);
    }
    // Closed or reset before a whole answer: the same cloud again, a second later.
    let broken: [fn() -> Reply; 3] = [
        || Reply::Answer(Vec::new()),
        || Reply::Answer(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhalf".to_vec()),
        || Reply::Reset(b"HTTP/1.1 200 OK\r\n".to_vec()),
    ];
    for reply in broken {
        let mut clock = FakeClock::default();
        let first = Rc::new(Cell::new(true));
        let mut network = FakeNetwork::serving(&clock, move |_, sent| {
            if first.replace(false) {
                reply()
            } else {
                azure(sent)
            }
        });
        assert_eq!(
            find(&mut network, &mut clock, 0).unwrap().cloud,
            Cloud::Azure
        );
        assert_eq!(network.paths(), [AZURE, AZURE]);
        assert_eq!(network.attempts, [0, SECOND]);
    }
}

#[test]
fn a_redirect_is_another_answer_and_is_not_followed() {
    let mut clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| match sent.path.as_str() {
        AZURE => Reply::Answer(
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        ),
        "/elsewhere" => azure(sent),
        _ => openstack(sent),
    });
    assert_eq!(
        find(&mut network, &mut clock, 0).unwrap().cloud,
        Cloud::OpenStack
    );
    assert_eq!(network.paths(), [AZURE, META, USER]);
}

#[test]
fn the_requests_and_the_rules_are_the_design_s() {
    assert_eq!(instance::ADDRESS.octets(), [169, 254, 169, 254]);
    assert_eq!(instance::PORT, 80);
    assert_eq!(instance::LIMIT_NS, 90 * SECOND);
    assert_eq!(instance::RETRY_NS, SECOND);
    assert_eq!(instance::REQUESTS_A_SECOND, 5);
    assert_eq!(
        String::from_utf8(instance::AZURE.encode()).unwrap(),
        "GET /metadata/instance?api-version=2021-02-01 HTTP/1.1\r\n\
         Host: 169.254.169.254\r\nConnection: close\r\nMetadata: true\r\n\r\n"
    );
    assert_eq!(instance::OPENSTACK.path(), META);
    assert_eq!(instance::OPENSTACK_USER_DATA.path(), USER);
    let lines = vec![
        instance::OPENSTACK.encode(),
        instance::OPENSTACK_USER_DATA.encode(),
    ];
    for line in lines {
        let text = String::from_utf8(line).unwrap();
        assert!(text.starts_with("GET /openstack/latest/"));
        assert!(!text.contains("Metadata"));
    }
}
