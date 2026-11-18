//! Azure's ready report (item 1.12): the goal state read as Microsoft's example reads it, the
//! `Health` document it writes, the two requests on the wire, and the retries every 5 s until
//! the 2 minutes are up.

use std::{
    cell::Cell,
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use super::{
    FakeClock, FakeNetwork, RECORDED_CONTAINER, RECORDED_INCARNATION, RECORDED_ROLE_INSTANCE,
    Reply, SECOND, Sent, answer, chunked, recorded, recorded_health, wire_server,
};
use crate::{
    boot::Stage,
    http::Request,
    machine::{Clock, StreamError},
    wireserver::{
        ADDRESS, GOAL_STATE, GoalState, HEADERS, HEALTH_HEADERS, HEALTH_PATH, LIMIT_NS, PORT,
        RETRY_NS, VALUE_MAX, report,
    },
    xml,
};

/// A goal state with these three values, at their documented paths, under a root of its own.
fn goal_state(incarnation: &str, container: &str, instance: &str) -> String {
    format!(
        "<GoalState><Incarnation>{incarnation}</Incarnation><Container><ContainerId>{container}\
         </ContainerId><RoleInstanceList><RoleInstance><InstanceId>{instance}</InstanceId>\
         </RoleInstance></RoleInstanceList></Container></GoalState>"
    )
}

fn parse(document: &str) -> Result<GoalState, String> {
    GoalState::parse(document.as_bytes())
}

fn values(state: &GoalState) -> (&str, &str, &str) {
    (&state.incarnation, &state.container, &state.instance)
}

/// The body of a recorded answer.
fn body_of(wire: &[u8]) -> &[u8] {
    let at = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    &wire[at + 4..]
}

#[test]
fn the_recorded_goal_state_names_its_container_and_role_instance() {
    let state = GoalState::parse(body_of(&recorded::goal_state())).unwrap();
    assert_eq!(
        values(&state),
        (
            RECORDED_INCARNATION,
            RECORDED_CONTAINER,
            RECORDED_ROLE_INSTANCE
        )
    );
}

#[test]
fn the_health_document_is_microsoft_s() {
    // As the Python of "Create Linux images without a provisioning agent" writes it.
    let state = GoalState {
        incarnation: "1".to_string(),
        container: "7b3e5a90-2c4d-4e1f-8a6b-9c0d1e2f3a4b".to_string(),
        instance: "c1d2e3f4-a5b6-4c7d-8e9f-0a1b2c3d4e5f._node".to_string(),
    };
    assert_eq!(
        state.health(),
        "<Health><GoalStateIncarnation>1</GoalStateIncarnation><Container><ContainerId>\
         7b3e5a90-2c4d-4e1f-8a6b-9c0d1e2f3a4b</ContainerId><RoleInstanceList><Role><InstanceId>\
         c1d2e3f4-a5b6-4c7d-8e9f-0a1b2c3d4e5f._node</InstanceId><Health><State>Ready</State>\
         </Health></Role></RoleInstanceList></Container></Health>"
    );
    let recorded = GoalState::parse(body_of(&recorded::goal_state())).unwrap();
    assert_eq!(recorded.health(), recorded_health());
    // What a value holds that the document's syntax uses is escaped, as ElementTree does it.
    let odd = parse(&goal_state("2", "a&amp;b", "x&lt;y&gt;z&quot;'")).unwrap();
    assert_eq!(values(&odd), ("2", "a&b", "x<y>z\"'"));
    assert!(odd.health().contains("<ContainerId>a&amp;b</ContainerId>"));
    assert!(
        odd.health()
            .contains("<InstanceId>x&lt;y&gt;z\"'</InstanceId>")
    );
}

#[test]
fn the_goal_state_is_read_as_findtext_reads_it() {
    // The documented paths, under any root.
    let state = parse(&goal_state("3", "c", "i").replace("GoalState", "Anything")).unwrap();
    assert_eq!(values(&state), ("3", "c", "i"));

    // Blank space around a value is not the value's, whatever its kind.
    let state = parse(&goal_state(" \r\n\t4\r\n ", "\nc\n", "  i  ")).unwrap();
    assert_eq!(values(&state), ("4", "c", "i"));

    // The first role instance is the one, as findtext takes it; and the first Incarnation.
    let two = "<GoalState><Incarnation>5</Incarnation><Incarnation>6</Incarnation><Container>\
        <ContainerId>c</ContainerId><RoleInstanceList><RoleInstance><InstanceId>first\
        </InstanceId></RoleInstance><RoleInstance><InstanceId>second</InstanceId></RoleInstance>\
        </RoleInstanceList></Container></GoalState>";
    assert_eq!(values(&parse(two).unwrap()), ("5", "c", "first"));

    // An element's text is what comes before its first child; comments, processing
    // instructions, CDATA, character references, attributes and empty elements are read past.
    let rich = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- a goal state -->\n\
        <GoalState xmlns:xsi='http://www.w3.org/2001/XMLSchema-instance' note=\"a>b\">\
        <Version>2012-11-30</Version><Incarnation>1<!-- one -->2<?pi x?></Incarnation>\
        <Machine/><Container note = 'x/>y' ><ContainerId><![CDATA[c<d]]>&#x26;&#38;</ContainerId>\
        <RoleInstanceList><RoleInstance><InstanceId>i<Child/>after the child</InstanceId>\
        </RoleInstance></RoleInstanceList></Container ></GoalState>\n<!-- after -->\n";
    let state = parse(rich).unwrap();
    assert_eq!(values(&state), ("12", "c<d&&", "i"));
}

#[test]
fn a_goal_state_without_its_three_values_is_refused() {
    let refused = |document: &str| parse(document).unwrap_err();
    assert_eq!(
        refused("<GoalState><Incarnation>1</Incarnation></GoalState>"),
        "has no Container/ContainerId"
    );
    assert_eq!(
        refused(&goal_state("1", "c", "i").replace("RoleInstance>", "Role>")),
        "has no Container/RoleInstanceList/RoleInstance/InstanceId"
    );
    // At the root, or deeper than the path, is not at the path.
    assert_eq!(
        refused("<Incarnation>1</Incarnation>"),
        "has no Incarnation"
    );
    assert_eq!(
        refused(&goal_state("1", "c", "i").replace(
            "<Incarnation>1</Incarnation>",
            "<Machine><Incarnation>1</Incarnation></Machine>"
        )),
        "has no Incarnation"
    );
    assert_eq!(
        refused(&goal_state("", "c", "i")),
        "has an empty Incarnation"
    );
    assert_eq!(
        refused(&goal_state("1", " \r\n ", "i")),
        "has an empty Container/ContainerId"
    );
    assert_eq!(
        refused(&goal_state("1", "c", "i").replace("<InstanceId>i</InstanceId>", "<InstanceId/>")),
        "has an empty Container/RoleInstanceList/RoleInstance/InstanceId"
    );
    // A value fits a document of its own: printable ASCII with no space, and short.
    let longest = "x".repeat(VALUE_MAX);
    assert_eq!(
        parse(&goal_state("1", &longest, "i")).unwrap().container,
        longest
    );
    assert_eq!(
        refused(&goal_state("1", &format!("{longest}x"), "i")),
        "has a Container/ContainerId over 256 bytes"
    );
    for odd in ["a b", "caf\u{e9}", "a&#9;b", "bell&#7;"] {
        assert_eq!(
            refused(&goal_state("1", "c", odd)),
            "has a Container/RoleInstanceList/RoleInstance/InstanceId that is not printable \
             ASCII without spaces",
            "{odd}"
        );
    }
}

#[test]
fn what_is_not_xml_is_refused() {
    let refused = |document: &[u8]| GoalState::parse(document).unwrap_err();
    let reason = |why: &str| format!("is not XML this node reads: {why}");
    let good = goal_state("1", "c", "i");
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"\xff\xfe<a/>".to_vec(), "it is not UTF-8"),
        (b"".to_vec(), "it has no root element"),
        (b"  \r\n ".to_vec(), "it has no root element"),
        (
            b"<?xml version=\"1.0\"?><!-- only -->".to_vec(),
            "it has no root element",
        ),
        (b"not xml".to_vec(), "it has text outside its root element"),
        (
            format!("{good}trailing").into_bytes(),
            "it has text outside its root element",
        ),
        (
            b"<![CDATA[x]]><a/>".to_vec(),
            "it has text outside its root element",
        ),
        (
            format!("{good}{good}").into_bytes(),
            "it has more than one root element",
        ),
        (
            format!("<!DOCTYPE x [<!ENTITY e \"i\">]>{good}").into_bytes(),
            "it has a DOCTYPE or a declaration this reader does not read",
        ),
        (
            good.replace("</Container>", "</Contained>").into_bytes(),
            "an end tag does not match its start tag",
        ),
        (b"</a>".to_vec(), "an end tag closes nothing"),
        (
            good.replace("</GoalState>", "").into_bytes(),
            "it ends inside an element",
        ),
        (
            good.replace("</GoalState>", "tail").into_bytes(),
            "it ends inside an element",
        ),
        (
            b"<a>&nbsp;</a>".to_vec(),
            "an entity is not one this reader knows",
        ),
        (
            b"<a>&#xD800;</a>".to_vec(),
            "an entity is not one this reader knows",
        ),
        (
            b"<a>&#xzz;</a>".to_vec(),
            "an entity is not one this reader knows",
        ),
        (b"<a>& b</a>".to_vec(), "an entity does not end"),
        (b"<a><!-- open</a>".to_vec(), "a comment does not end"),
        (
            b"<a><?pi</a>".to_vec(),
            "a processing instruction does not end",
        ),
        (
            b"<a><![CDATA[open</a>".to_vec(),
            "a CDATA section does not end",
        ),
        (
            b"<a attribute=\"open></a>".to_vec(),
            "a start tag does not end",
        ),
        (b"<a></a".to_vec(), "an end tag does not end"),
        (b"<1a></1a>".to_vec(), "a tag's name is not an XML name"),
        (b"<></>".to_vec(), "a tag's name is not an XML name"),
        (b"<a-></a->".to_vec(), "has no Incarnation"),
    ];
    for (document, why) in cases {
        let expected = if why.starts_with("has ") {
            why.to_string()
        } else {
            reason(why)
        };
        assert_eq!(
            refused(&document),
            expected,
            "{}",
            String::from_utf8_lossy(&document)
        );
    }
    // Elements nest at most 64 deep.
    let deep = |depth: usize| format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
    assert_eq!(
        xml::find_texts(deep(xml::DEPTH_MAX).as_bytes(), [&["a"][..]]),
        Ok([Some(String::new())])
    );
    assert_eq!(
        xml::find_texts(deep(xml::DEPTH_MAX + 1).as_bytes(), [&["a"][..]]),
        Err("its elements nest too deep")
    );
}

#[test]
fn the_report_is_the_goal_state_then_the_health_document() {
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| wire_server(sent));
    report(&mut network, &mut clock.clone()).unwrap();

    // Each on a connection of its own to the WireServer.
    let sent: Vec<&Sent> = network.requests.iter().map(|(_, sent)| sent).collect();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|sent| sent.to == (ADDRESS, PORT)));
    assert_eq!(ADDRESS.octets(), [168, 63, 129, 16]);
    assert_eq!(PORT, 80);
    // The goal state: a GET with the one header 1F names, besides Host and Connection.
    assert_eq!(
        GOAL_STATE.encode(),
        b"GET /machine?comp=goalstate HTTP/1.1\r\nHost: 168.63.129.16\r\nConnection: close\r\n\
          x-ms-version: 2012-11-30\r\n\r\n"
    );
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("GET", "/machine?comp=goalstate")
    );
    // The Health document: a POST with the same header, the document's type, this agent's
    // name (PM, item 1.12), and the document's length.
    let health = recorded_health();
    assert_eq!(
        sent[1].headers,
        [
            ("Host".to_string(), "168.63.129.16".to_string()),
            ("Connection".to_string(), "close".to_string()),
            ("x-ms-version".to_string(), "2012-11-30".to_string()),
            (
                "Content-Type".to_string(),
                "text/xml;charset=utf-8".to_string()
            ),
            ("x-ms-agent-name".to_string(), "moruna".to_string()),
            ("Content-Length".to_string(), health.len().to_string()),
        ]
    );
    assert_eq!(
        (sent[1].method.as_str(), sent[1].path.as_str()),
        ("POST", HEALTH_PATH)
    );
    assert_eq!(sent[1].body, health.as_bytes());
    assert_eq!(HEADERS, [("x-ms-version", "2012-11-30")]);
    assert_eq!(HEALTH_HEADERS[0], HEADERS[0]);
    // It took no time on the clock beyond the requests'.
    assert_eq!(clock.now(), 0);
}

/// A WireServer that answers each request with the next of `replies`, then as the real one.
fn scripted(replies: Vec<Reply>) -> impl FnMut(u64, &Sent) -> Reply + 'static {
    let mut replies = replies.into_iter();
    move |_, sent| replies.next().unwrap_or_else(|| wire_server(sent))
}

#[test]
fn a_refused_report_is_tried_again_every_5_s() {
    let clock = FakeClock::default();
    clock.advance(7 * SECOND);
    let goal_state_without_container = answer(
        "200 OK",
        b"<GoalState><Incarnation>1</Incarnation></GoalState>",
    );
    let mut network = FakeNetwork::serving(
        &clock,
        scripted(vec![
            // The goal state is refused.
            Reply::Answer(answer("503 Service Unavailable", b"")),
            // The goal state is taken, the Health document refused.
            Reply::Answer(recorded::goal_state()),
            Reply::Answer(answer("500 Internal Server Error", b"")),
            // A goal state that does not name the container.
            Reply::Answer(goal_state_without_container),
            // A goal state that breaks off.
            Reply::Reset(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n<Goal".to_vec()),
        ]),
    );
    report(&mut network, &mut clock.clone()).unwrap();
    // Five attempts, 5 s apart from the report's start; the last both requests.
    let at: Vec<u64> = network.attempts.iter().map(|at| at / SECOND).collect();
    assert_eq!(at, [7, 12, 12, 17, 22, 27, 27]);
    assert_eq!(
        network.requests.last().unwrap().1.body,
        recorded_health().as_bytes()
    );
    assert_eq!(RETRY_NS, 5 * SECOND);
}

#[test]
fn a_wireserver_that_cannot_be_reached_is_tried_again() {
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, sent| wire_server(sent)).connecting(|now| {
        match now / SECOND {
            0 => Err((StreamError::Refused, 0)),
            5 => Err((StreamError::TimedOut, 3 * SECOND)),
            _ => Ok(()),
        }
    });
    report(&mut network, &mut clock.clone()).unwrap();
    let at: Vec<u64> = network.attempts.iter().map(|at| at / SECOND).collect();
    assert_eq!(at, [0, 5, 10, 10]);
}

#[test]
fn an_attempt_has_until_the_next_one_begins() {
    // A WireServer that says nothing to the first attempt: it is given up at 5 s, when the
    // next one begins.
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, scripted(vec![Reply::Stalls]));
    report(&mut network, &mut clock.clone()).unwrap();
    let at: Vec<u64> = network.attempts.iter().map(|at| at / SECOND).collect();
    assert_eq!(at, [0, 5, 5]);
}

#[test]
fn a_report_not_taken_within_2_minutes_is_fatal() {
    // Always refused: an attempt at 0, 5, ... 115 s, and at 120 s the fatal line.
    let clock = FakeClock::default();
    clock.advance(30 * SECOND);
    let mut network = FakeNetwork::serving(&clock, |_, _| {
        Reply::Answer(answer("503 Service Unavailable", b""))
    });
    let fatal = report(&mut network, &mut clock.clone()).unwrap_err();
    assert_eq!(fatal.stage, Stage::Platform);
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: Azure did not take the ready report within 120 s: GET \
         /machine?comp=goalstate: answered 503"
    );
    let expected: Vec<u64> = (0..24).map(|n| 30 * SECOND + n * RETRY_NS).collect();
    assert_eq!(network.attempts, expected);
    assert_eq!(clock.now(), 30 * SECOND + LIMIT_NS);
    assert_eq!(LIMIT_NS, 120 * SECOND);

    // A Health document never taken says so.
    let clock = FakeClock::default();
    let count = Rc::new(Cell::new(0));
    let seen = count.clone();
    let mut network = FakeNetwork::serving(&clock, move |_, sent| {
        seen.set(seen.get() + 1);
        match sent.method.as_str() {
            "GET" => wire_server(sent),
            _ => Reply::Answer(answer("400 Bad Request", b"")),
        }
    });
    let fatal = report(&mut network, &mut clock.clone()).unwrap_err();
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: Azure did not take the ready report within 120 s: POST \
         /machine?comp=health: answered 400"
    );
    assert_eq!(count.get(), 48);

    // A WireServer that never answers: each attempt waits out its 5 s.
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| Reply::Stalls);
    let fatal = report(&mut network, &mut clock.clone()).unwrap_err();
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: Azure did not take the ready report within 120 s: GET \
         /machine?comp=goalstate: the deadline passed"
    );
    assert_eq!(network.attempts.len(), 24);
    assert_eq!(clock.now(), LIMIT_NS);

    // And a goal state that does not name its values says why.
    let clock = FakeClock::default();
    let mut network = FakeNetwork::serving(&clock, |_, _| {
        Reply::Answer(answer("200 OK", b"<GoalState/>"))
    });
    let fatal = report(&mut network, &mut clock.clone()).unwrap_err();
    assert_eq!(
        fatal.to_string(),
        "moruna: fatal platform: Azure did not take the ready report within 120 s: GET \
         /machine?comp=goalstate: the goal state has no Incarnation"
    );
}

#[test]
fn the_requests_are_what_the_client_sends() {
    // The Health document is posted as Request::post encodes it.
    let health = recorded_health();
    let post = Request::post(
        "168.63.129.16",
        HEALTH_PATH,
        HEALTH_HEADERS,
        health.as_bytes(),
    );
    let wire = post.encode();
    let head = format!(
        "POST /machine?comp=health HTTP/1.1\r\nHost: 168.63.129.16\r\nConnection: close\r\n\
         x-ms-version: 2012-11-30\r\nContent-Type: text/xml;charset=utf-8\r\n\
         x-ms-agent-name: moruna\r\nContent-Length: {}\r\n\r\n",
        health.len()
    );
    assert_eq!(&wire[..head.len()], head.as_bytes());
    assert_eq!(&wire[head.len()..], health.as_bytes());
}

#[test]
fn a_wireserver_that_answers_in_chunks_takes_the_report() {
    // Every answer in chunks, of a few sizes: the goal state is read whole all the same.
    for size in [1, 7, 512, 1 << 20] {
        let clock = FakeClock::default();
        let mut network = FakeNetwork::serving(&clock, move |_, sent| match wire_server(sent) {
            Reply::Answer(wire) => Reply::Answer(chunked(&wire, size)),
            other => other,
        });
        network.chunk = 5;
        report(&mut network, &mut clock.clone()).unwrap();
        assert_eq!(network.requests.len(), 2, "in chunks of {size}");
        assert_eq!(network.attempts, [0, 0]);
    }
}
