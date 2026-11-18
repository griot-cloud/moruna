//! The job operations end to end, over TLS, against the tests' engine (item 3.2, phase 1): a
//! job validated, submitted, followed over Server-Sent Events and reported; busy, cancel and
//! the engine's refusals; `?from=`, after which a stream replays; the scopes; MCP, the events'
//! page polled to the end; and a credential that appears nowhere.
mod common;

use common::{Harness, Response, WALL, engine::RUN};
use moruna_api_server::IDLE_LIMIT_MS;
use serde_json::{Value, json};

const JOB: &str =
    r#"{"v1":{"moruna_spec":1,"source":{"kind":"rows","rows":1000},"sink":{"kind":"count"}}}"#;

/// What client `index` has received and not yet read, as text.
fn received(harness: &mut Harness, index: usize) -> String {
    String::from_utf8(std::mem::take(&mut harness.clients[index].received)).unwrap()
}

/// The Server-Sent Events in `text`: each one's ID, name and data.
fn events(text: &str) -> Vec<(Option<u64>, String, Value)> {
    text.split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| {
            let mut id = None;
            let (mut name, mut data) = (String::new(), Value::Null);
            for line in event.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    id = Some(value.parse().unwrap());
                } else if let Some(value) = line.strip_prefix("event: ") {
                    name = value.to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(value).expect("data is one line of JSON");
                } else {
                    panic!("not a line of an event: {line:?}");
                }
            }
            (id, name, data)
        })
        .collect()
}

/// Submits [`JOB`] on a new connection and has the engine take it: the run's ID is [`RUN`].
fn submit(harness: &mut Harness) -> usize {
    let client = harness.connect();
    let token = harness.token(&["submit"]);
    harness.send(client, &Harness::post("/v1/jobs", &token, JOB));
    assert!(
        harness.clients[client].response().is_none(),
        "the submit waits for the engine"
    );
    harness.engine.attempt_started();
    harness.settle();
    let response = harness.clients[client].response().unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.json(), json!({ "id": RUN }));
    client
}

/// Opens a stream of the job's events on a new connection, and gives the connection.
fn follow(harness: &mut Harness, query: &str) -> usize {
    let client = harness.connect();
    let token = harness.token(&["observe"]);
    let request = format!(
        "GET /v1/jobs/{RUN}/events{query} HTTP/1.1\r\nHost: node\r\n{}\r\n",
        Harness::bearer(&token)
    );
    harness.send(client, request.as_bytes());
    let text = received(harness, client);
    let (head, rest) = text.split_once("\r\n\r\n").expect("a response head");
    assert_eq!(
        head,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close"
    );
    harness.clients[client].received = rest.as_bytes().to_vec();
    client
}

#[test]
fn a_job_is_validated_submitted_followed_and_reported() {
    let mut harness = Harness::new();
    let submitter = harness.token(&["submit"]);
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let client = harness.connect();
    // Validate: the engine's check, and nothing runs.
    let response = harness.exchange(client, &Harness::post("/v1/jobs/validate", &submitter, JOB));
    assert_eq!(response.status, 200);
    assert_eq!(response.json(), json!({ "ok": true }));
    assert!(harness.engine.0.borrow().started.is_empty());
    assert!(harness.engine.0.borrow().checked[0].starts_with(br#"{"moruna_spec":1"#));
    // Submit: a fresh engine with the document as its spec.
    submit(&mut harness);
    assert_eq!(
        harness.engine.spec(),
        json!({ "type": "spec", "spec": serde_json::from_str::<Value>(JOB).unwrap()["v1"] })
    );
    let status = harness.get(client, &format!("/v1/jobs/{RUN}"), &observer);
    assert_eq!(
        status.json(),
        json!({ "id": RUN, "state": "starting", "submitted": WALL, "submitted_ms": WALL * 1_000 })
    );
    // Follow it: what the buffer holds, then each thing as the engine says it.
    let stream = follow(&mut harness, "");
    let first = events(&received(&mut harness, stream));
    assert_eq!(first.len(), 1);
    assert_eq!((first[0].0, first[0].1.as_str()), (Some(1), "event"));
    assert_eq!(first[0].2["kind"], "attempt_started");
    assert_eq!(first[0].2["type"], "event");
    harness.clock.advance(1_987);
    harness.engine.heartbeat(500);
    harness.engine.log("source", "made 500 rows");
    harness.settle();
    let next = events(&received(&mut harness, stream));
    assert_eq!(next.len(), 2);
    assert_eq!((next[0].0, next[0].1.as_str()), (Some(2), "event"));
    assert_eq!(
        (&next[0].2["kind"], &next[0].2["text"]),
        (&json!("log"), &json!("made 500 rows"))
    );
    assert_eq!((next[1].0, next[1].1.as_str()), (None, "heartbeat"));
    assert_eq!(next[1].2["rows_out"], 500);
    let status = harness
        .get(client, &format!("/v1/jobs/{RUN}"), &observer)
        .json();
    // 1,987 ms on: the millisecond, and its whole second, 1.
    assert_eq!(
        (&status["state"], &status["started"], &status["started_ms"]),
        (
            &json!("running"),
            &json!(WALL + 1),
            &json!(WALL * 1_000 + 1_987)
        )
    );
    assert_eq!(status["heartbeat"]["rows_out"], 500);
    let report = harness.get(client, &format!("/v1/jobs/{RUN}/report"), &observer);
    assert_eq!(
        (report.status, report.error_code().as_str()),
        (409, "conflict")
    );
    // The end: the report, the exit, and the stream closes.
    harness.clock.advance(3_000);
    harness.engine.heartbeat(1000);
    harness.engine.finish(
        r#"{"rows_out":1000,"stages":[{"name":"count","rows":1000}]}"#,
        0,
        None,
    );
    harness.settle();
    let last = events(&received(&mut harness, stream));
    let names: Vec<&str> = last.iter().map(|event| event.1.as_str()).collect();
    assert_eq!(names, ["heartbeat", "report", "exit"]);
    assert_eq!(last[1].2["report"]["rows_out"], 1000);
    assert_eq!(
        last[2].2,
        json!({ "type": "exit", "code": 0, "diagnostic": null })
    );
    assert!(harness.clients[stream].closed_by_server && harness.clients[stream].server_closed());
    let status = harness
        .get(client, &format!("/v1/jobs/{RUN}"), &observer)
        .json();
    assert_eq!(
        (
            &status["state"],
            &status["ended"],
            &status["ended_ms"],
            &status["exit_code"]
        ),
        (
            &json!("completed"),
            &json!(WALL + 4),
            &json!(WALL * 1_000 + 4_987),
            &json!(0)
        )
    );
    let report = harness.get(client, &format!("/v1/jobs/{RUN}/report"), &observer);
    assert_eq!(report.status, 200);
    assert_eq!(
        report.body,
        br#"{"rows_out":1000,"stages":[{"name":"count","rows":1000}]}"#
    );
    let list = harness.get(client, "/v1/jobs", &observer).json();
    assert_eq!(
        list,
        json!({ "jobs": [{ "id": RUN, "state": "completed",
                           "submitted": WALL, "submitted_ms": WALL * 1_000,
                           "started": WALL + 1, "started_ms": WALL * 1_000 + 1_987,
                           "ended": WALL + 4, "ended_ms": WALL * 1_000 + 4_987 }] })
    );
    // Every call left its record, with the status it was answered with.
    let admin = harness.token(&["admin"]);
    let audit = harness
        .get(client, "/v1/node/audit", &Harness::bearer(&admin))
        .json();
    let mut calls: Vec<(String, u64)> = audit["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["operation"].as_str().unwrap().to_string(),
                r["status"].as_u64().unwrap(),
            )
        })
        .collect();
    calls.reverse();
    assert_eq!(
        calls,
        [
            ("jobs_validate", 200),
            ("jobs_submit", 201),
            ("jobs_get", 200),
            ("jobs_events", 200),
            ("jobs_get", 200),
            ("jobs_report", 409),
            ("jobs_get", 200),
            ("jobs_report", 200),
            ("jobs_list", 200),
        ]
        .map(|(operation, status)| (operation.to_string(), status))
    );
}

#[test]
fn a_second_submit_while_a_job_runs_is_busy() {
    let mut harness = Harness::new();
    submit(&mut harness);
    let token = harness.token(&["submit"]);
    let client = harness.connect();
    let response = harness.exchange(client, &Harness::post("/v1/jobs", &token, JOB));
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (503, "busy")
    );
    assert_eq!(harness.engine.0.borrow().started.len(), 1);
    // Once it has ended, the next is taken.
    harness.engine.exit(0, None);
    harness.settle();
    harness.engine.run_as("11111111111111111111111111111111");
    let client = harness.connect();
    harness.send(client, &Harness::post("/v1/jobs", &token, JOB));
    harness.engine.attempt_started();
    harness.settle();
    let response = harness.clients[client].response().unwrap();
    assert_eq!(
        response.json(),
        json!({ "id": "11111111111111111111111111111111" })
    );
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let list = harness.get(client, "/v1/jobs", &observer).json();
    let ids: Vec<&str> = list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["11111111111111111111111111111111", RUN],
        "newest first"
    );
}

#[test]
fn a_large_report_goes_in_pieces_whole_over_http_and_mcp_then_the_connection_closes() {
    let mut harness = Harness::new();
    submit(&mut harness);
    // A report of 300 KiB with characters JSON escapes: many pieces of an answer.
    let report = format!(r#"{{"pad":"{}"}}"#, r#"q\"\n"#.repeat(60 * 1024));
    harness.engine.finish(&report, 0, None);
    harness.settle();
    let observer = harness.token(&["observe"]);
    let client = harness.connect();
    let request = format!(
        "GET /v1/jobs/{RUN}/report HTTP/1.1\r\nHost: node\r\n{}Connection: close\r\n\r\n",
        Harness::bearer(&observer)
    );
    let response = harness.exchange(client, request.as_bytes());
    assert_eq!(response.status, 200);
    assert_eq!(response.body, report.as_bytes());
    assert_eq!(response.header("connection"), Some("close"));
    assert!(harness.clients[client].server_closed());
    // Over MCP: the same report as the text, escaped, and as the structured content.
    let client = harness.connect();
    let call = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                       "params": { "name": "moruna_jobs_report", "arguments": { "id": RUN } } });
    let response = harness.exchange(client, &Harness::post("/mcp", &observer, &call.to_string()));
    let result = result(&response);
    let expected: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(result["structuredContent"], expected);
    assert_eq!(result["content"][0]["text"], report.as_str());
    assert_eq!(
        response.body.len(),
        response
            .header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap()
    );
}

#[test]
fn a_long_job_is_cancelled() {
    let mut harness = Harness::new();
    let client = submit(&mut harness);
    harness.engine.heartbeat(10);
    let token = harness.token(&["submit"]);
    let cancel = Harness::post(&format!("/v1/jobs/{RUN}/cancel"), &token, "");
    let response = harness.exchange(client, &cancel);
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["state"], "running");
    assert_eq!(harness.engine.0.borrow().sent, ["{\"type\":\"cancel\"}\n"]);
    harness.engine.exit(130, Some("cancelled"));
    harness.settle();
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let status = harness
        .get(client, &format!("/v1/jobs/{RUN}"), &observer)
        .json();
    assert_eq!(
        (&status["state"], &status["exit_code"]),
        (&json!("cancelled"), &json!(130))
    );
    let again = harness.exchange(client, &cancel);
    assert_eq!(
        (again.status, again.error_code().as_str()),
        (409, "conflict")
    );
    let missing = Harness::post(
        "/v1/jobs/ffffffffffffffffffffffffffffffff/cancel",
        &token,
        "",
    );
    assert_eq!(harness.exchange(client, &missing).status, 404);
    for path in [
        "/v1/jobs/ffff",
        "/v1/jobs/ffff/report",
        "/v1/jobs/ffff/events",
    ] {
        let response = harness.get(client, path, &observer);
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (404, "not_found")
        );
    }
}

#[test]
fn an_invalid_job_is_refused_at_validate_and_at_submit_with_its_field() {
    let mut harness = Harness::new();
    let token = harness.token(&["submit"]);
    harness
        .engine
        .0
        .borrow_mut()
        .verdicts
        .push_back(Ok(vec![moruna_api::Refusal {
            field: "source.kind".into(),
            message: "no source kind is named `rows`".into(),
        }]));
    let client = harness.connect();
    let response = harness.exchange(client, &Harness::post("/v1/jobs/validate", &token, JOB));
    assert_eq!(response.status, 200);
    assert_eq!(
        response.json(),
        json!({ "ok": false, "refusals": [{ "field": "source.kind",
                                             "message": "no source kind is named `rows`" }] })
    );
    // At submit, the engine itself refuses: exit code 2, with the refusal.
    harness.send(client, &Harness::post("/v1/jobs", &token, JOB));
    harness
        .engine
        .exit(2, Some("source.kind: no source kind is named `rows`"));
    harness.settle();
    let response = harness.clients[client].response().unwrap();
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (400, "invalid")
    );
    assert_eq!(
        response.json()["error"]["message"],
        "source.kind: no source kind is named `rows`"
    );
    let observer = Harness::bearer(&harness.token(&["observe"]));
    assert_eq!(
        harness.get(client, "/v1/jobs", &observer).json(),
        json!({ "jobs": [] })
    );
    // A body that is no job never reaches the engine.
    let response = harness.exchange(client, &Harness::post("/v1/jobs", &token, r#"{"v3":{}}"#));
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (400, "invalid")
    );
    assert_eq!(harness.engine.0.borrow().started.len(), 1);
}

#[test]
fn the_job_operations_need_their_scopes() {
    let mut harness = Harness::new();
    let submitter = submit(&mut harness);
    let observe = harness.token(&["observe"]);
    // observe reads, and neither validates, submits nor cancels; refused on the head, a body
    // is never read.
    for path in ["/v1/jobs/validate", "/v1/jobs"] {
        let client = harness.connect();
        let response = harness.exchange(client, &Harness::post(path, &observe, JOB));
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (403, "forbidden")
        );
        assert_eq!(response.header("connection"), Some("close"));
    }
    let client = harness.connect();
    let cancel = Harness::post(&format!("/v1/jobs/{RUN}/cancel"), &observe, "");
    assert_eq!(harness.exchange(client, &cancel).status, 403);
    assert!(harness.engine.0.borrow().sent.is_empty());
    for path in ["/v1/jobs".to_string(), format!("/v1/jobs/{RUN}")] {
        assert_eq!(
            harness
                .get(client, &path, &Harness::bearer(&observe))
                .status,
            200
        );
        assert_eq!(harness.get(client, &path, "").status, 401);
    }
    // Another method on a job path is invalid, and a body where none belongs.
    let put = format!("PUT /v1/jobs/{RUN} HTTP/1.1\r\nHost: node\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(harness.exchange(submitter, put.as_bytes()).status, 400);
    let submit = harness.token(&["submit"]);
    let with_body = Harness::post(&format!("/v1/jobs/{RUN}/cancel"), &submit, "{}");
    let response = harness.exchange(submitter, &with_body);
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (400, "invalid")
    );
}

#[test]
fn from_replays_after_that_record_and_nothing_else_is_a_query() {
    let mut harness = Harness::new();
    submit(&mut harness);
    for n in 0..4 {
        harness.engine.log("source", &format!("line {n}"));
    }
    harness.settle();
    // `from` is the last record the reader has, as Last-Event-ID is: the stream goes on after it.
    let stream = follow(&mut harness, "?from=3");
    let replayed = events(&received(&mut harness, stream));
    let ids: Vec<Option<u64>> = replayed.iter().map(|event| event.0).collect();
    assert_eq!(ids, [Some(4), Some(5)]);
    assert_eq!(replayed[0].2["text"], "line 2");
    // An ended job replays to its end; from 0, all of it, as with no from.
    harness.engine.finish("{}", 0, None);
    harness.settle();
    let whole = follow(&mut harness, "");
    let all = events(&received(&mut harness, whole));
    assert_eq!(all.len(), 7, "five records, the report and the exit");
    assert!(harness.clients[whole].server_closed());
    let zero = follow(&mut harness, "?from=0");
    assert_eq!(events(&received(&mut harness, zero)), all);
    let last = follow(&mut harness, "?from=5");
    let end: Vec<String> = events(&received(&mut harness, last))
        .into_iter()
        .map(|event| event.1)
        .collect();
    assert_eq!(
        end,
        ["report", "exit"],
        "after the last record, only the end"
    );
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let client = harness.connect();
    for query in ["?from=", "?from=x", "?from=-1", "?to=3", "?from=1&x=2", "?"] {
        let response = harness.get(client, &format!("/v1/jobs/{RUN}/events{query}"), &observer);
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (400, "invalid"),
            "{query}"
        );
    }
    let response = harness.get(client, &format!("/v1/jobs/{RUN}?from=1"), &observer);
    assert_eq!(response.status, 400, "only the events take from");
}

#[test]
fn a_credential_put_in_a_job_appears_in_no_answer_event_log_or_report() {
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let job = format!(
        r#"{{"v1":{{"moruna_spec":1,"source":{{"kind":"s3"}},"sink":{{"kind":"count"}},"object_store":{{"s3":{{"secret_access_key":{{"secret":"aws"}}}}}}}},"secrets":{{"aws":"{SECRET}"}}}}"#
    );
    let mut harness = Harness::new();
    let token = harness.token(&["admin"]);
    let bearer = Harness::bearer(&token);
    let mut seen = String::new();
    let mut keep = |response: Response| seen.push_str(&String::from_utf8(response.body).unwrap());
    // Validate, with a refusal that quotes it.
    harness
        .engine
        .0
        .borrow_mut()
        .verdicts
        .push_back(Ok(vec![moruna_api::Refusal {
            field: "source.secret_access_key".into(),
            message: format!("`{SECRET}` is not a key the source takes"),
        }]));
    let client = harness.connect();
    keep(harness.exchange(client, &Harness::post("/v1/jobs/validate", &token, &job)));
    // Submit: the engine has it, in its spec.
    harness.send(client, &Harness::post("/v1/jobs", &token, &job));
    assert!(harness.engine.0.borrow().started[0].contains(SECRET));
    harness.engine.attempt_started();
    harness.settle();
    keep(harness.clients[client].response().unwrap());
    // The engine says it in a log, a heartbeat, its report and its exit.
    let stream = follow(&mut harness, "");
    harness
        .engine
        .log("source", &format!("signing with {SECRET}"));
    harness.engine.say(&format!(
        r#"{{"type":"heartbeat","t_ms":5,"rows_out":0,"bottleneck":"{SECRET}"}}"#
    ));
    harness.settle();
    keep(harness.get(client, &format!("/v1/jobs/{RUN}"), &bearer));
    harness.engine.finish(
        &format!(r#"{{"source":{{"secret_access_key":"{SECRET}"}}}}"#),
        4,
        Some(&format!("the source refused {SECRET}")),
    );
    harness.settle();
    let followed = received(&mut harness, stream);
    assert_eq!(events(&followed).len(), 5, "{followed}");
    seen.push_str(&followed);
    let replay = follow(&mut harness, "");
    seen.push_str(&received(&mut harness, replay));
    let mut keep = |response: Response| seen.push_str(&String::from_utf8(response.body).unwrap());
    keep(harness.get(client, &format!("/v1/jobs/{RUN}"), &bearer));
    keep(harness.get(client, &format!("/v1/jobs/{RUN}/report"), &bearer));
    keep(harness.get(client, "/v1/jobs", &bearer));
    keep(harness.get(client, "/v1/node/audit", &bearer));
    // Over MCP too, and a submit the engine refuses with it.
    for (tool, arguments) in [
        ("moruna_jobs_get", json!({ "id": RUN })),
        ("moruna_jobs_report", json!({ "id": RUN })),
        ("moruna_jobs_list", json!({})),
    ] {
        let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                           "params": { "name": tool, "arguments": arguments } });
        keep(harness.exchange(client, &Harness::post("/mcp", &token, &call.to_string())));
    }
    harness.send(client, &Harness::post("/v1/jobs", &token, &job));
    harness.engine.exit(
        2,
        Some(&format!("source.secret_access_key: {SECRET} is refused")),
    );
    harness.settle();
    keep(harness.clients[client].response().unwrap());
    assert!(!seen.contains(SECRET), "{seen}");
    assert!(seen.matches("[redacted]").count() >= 14, "{seen}");
}

/// A tool call's answer on client `index`: the JSON-RPC result.
fn tool(harness: &mut Harness, index: usize, token: &str, name: &str, arguments: Value) -> Value {
    let call = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                       "params": { "name": name, "arguments": arguments } });
    harness.send(index, &Harness::post("/mcp", token, &call.to_string()));
    match harness.clients[index].response() {
        Some(response) => result(&response),
        None => Value::Null,
    }
}

fn result(response: &Response) -> Value {
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["id"], 7);
    body["result"].clone()
}

#[test]
fn an_mcp_client_runs_a_job_through_the_tools() {
    let mut harness = Harness::new();
    let token = harness.token(&["submit"]);
    let client = harness.connect();
    let job: Value = serde_json::from_str(JOB).unwrap();
    // Validate and submit answer when the engine has: the call waits meanwhile.
    harness.engine.0.borrow_mut().hold_checks = true;
    assert_eq!(
        tool(
            &mut harness,
            client,
            &token,
            "moruna_jobs_validate",
            job.clone()
        ),
        Value::Null
    );
    harness.engine.0.borrow_mut().hold_checks = false;
    harness.settle();
    let validated = result(&harness.clients[client].response().unwrap());
    assert_eq!(validated["structuredContent"], json!({ "ok": true }));
    assert_eq!(
        tool(
            &mut harness,
            client,
            &token,
            "moruna_jobs_submit",
            job.clone()
        ),
        Value::Null
    );
    assert_eq!(
        harness.engine.spec()["spec"],
        job["v1"],
        "the document reaches the engine as the tool was given it"
    );
    harness.engine.attempt_started();
    harness.settle();
    let submitted = result(&harness.clients[client].response().unwrap());
    assert_eq!(submitted["structuredContent"], json!({ "id": RUN }));
    assert_eq!(
        submitted["content"][0]["text"],
        format!(r#"{{"id":"{RUN}"}}"#)
    );
    assert!(submitted.get("isError").is_none());
    // A second submit is the operation's error, as a tool error.
    let busy = tool(&mut harness, client, &token, "moruna_jobs_submit", job);
    assert_eq!(busy["isError"], true);
    assert!(
        busy["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(r#""code":"busy""#)
    );
    // The run's ID is each other tool's argument.
    let status = tool(
        &mut harness,
        client,
        &token,
        "moruna_jobs_get",
        json!({ "id": RUN }),
    );
    assert_eq!(status["structuredContent"]["state"], "starting");
    let listed = tool(&mut harness, client, &token, "moruna_jobs_list", json!({}));
    assert_eq!(listed["structuredContent"]["jobs"][0]["id"], RUN);
    let cancelled = tool(
        &mut harness,
        client,
        &token,
        "moruna_jobs_cancel",
        json!({ "id": RUN }),
    );
    assert_eq!(cancelled["structuredContent"]["id"], RUN);
    assert_eq!(harness.engine.0.borrow().sent.len(), 1);
    harness
        .engine
        .finish(r#"{"rows_out":3}"#, 130, Some("cancelled"));
    harness.settle();
    let report = tool(
        &mut harness,
        client,
        &token,
        "moruna_jobs_report",
        json!({ "id": RUN }),
    );
    assert_eq!(report["structuredContent"], json!({ "rows_out": 3 }));
    // What a tool does not take is its operation's `invalid`.
    for (name, arguments) in [
        ("moruna_jobs_get", json!({})),
        ("moruna_jobs_get", json!({ "id": 7 })),
        ("moruna_jobs_get", json!({ "id": RUN, "from": 1 })),
        ("moruna_jobs_list", json!({ "id": RUN })),
        ("moruna_jobs_validate", json!({ "v3": {} })),
        ("moruna_jobs_events", json!({ "from": 1 })),
    ] {
        let refused = tool(&mut harness, client, &token, name, arguments);
        assert_eq!(refused["isError"], true, "{name}");
        assert!(
            refused["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(r#""code":"invalid""#)
        );
    }
    // A tool no operation has is JSON-RPC's invalid params.
    let call = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                       "params": { "name": "moruna_job_events", "arguments": { "id": RUN } } });
    let response = harness.exchange(client, &Harness::post("/mcp", &token, &call.to_string()));
    assert_eq!(response.json()["error"]["code"], -32_602);
    // observe lists and reads, and does not submit: a tool error with forbidden.
    let observe = harness.token(&["observe"]);
    let forbidden = tool(
        &mut harness,
        client,
        &observe,
        "moruna_jobs_submit",
        serde_json::from_str(JOB).unwrap(),
    );
    assert_eq!(forbidden["isError"], true);
    assert!(
        forbidden["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(r#""code":"forbidden""#)
    );
    // Each tool call left one record, with its operation's ID.
    let admin = Harness::bearer(&harness.token(&["admin"]));
    let audit = harness.get(client, "/v1/node/audit", &admin).json();
    let operations: Vec<&str> = audit["records"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .map(|r| r["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        operations[..7],
        [
            "jobs_validate",
            "jobs_submit",
            "jobs_submit",
            "jobs_get",
            "jobs_list",
            "jobs_cancel",
            "jobs_report"
        ]
    );
}

/// One poll of the events' tool on client `index`, after the record `from`: the page's events
/// and its `next`.
fn poll(harness: &mut Harness, index: usize, token: &str, from: u64) -> (Vec<Value>, u64) {
    let result = tool(
        harness,
        index,
        token,
        "moruna_jobs_events",
        json!({ "id": RUN, "from": from }),
    );
    assert!(result.get("isError").is_none(), "{result}");
    let page = &result["structuredContent"];
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, page, "the text and the structured content agree");
    (
        page["records"].as_array().unwrap().clone(),
        page["next"].as_u64().unwrap(),
    )
}

/// Each event's name, and its number when it has one.
fn names(events: &[Value]) -> Vec<(String, Option<u64>)> {
    events
        .iter()
        .map(|event| {
            (
                event["event"].as_str().unwrap().to_string(),
                event["id"].as_u64(),
            )
        })
        .collect()
}

#[test]
fn an_mcp_client_follows_a_job_to_its_report_by_polling_its_events() {
    let mut harness = Harness::new();
    let submitter = harness.token(&["submit"]);
    let observer = harness.token(&["observe"]);
    let client = harness.connect();
    // The tool is listed, with what it takes.
    let list = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" });
    let listed = harness.exchange(client, &Harness::post("/mcp", &observer, &list.to_string()));
    let events_tool = listed.json()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "moruna_jobs_events")
        .cloned()
        .expect("the events' tool is listed");
    assert_eq!(events_tool["inputSchema"]["required"], json!(["id"]));
    assert!(events_tool["inputSchema"]["properties"]["from"].is_object());
    // Submit over MCP; the engine takes the job and tells its first record.
    let job: Value = serde_json::from_str(JOB).unwrap();
    assert_eq!(
        tool(&mut harness, client, &submitter, "moruna_jobs_submit", job),
        Value::Null
    );
    harness.engine.attempt_started();
    harness.settle();
    let submitted = result(&harness.clients[client].response().unwrap());
    assert_eq!(submitted["structuredContent"], json!({ "id": RUN }));
    // Poll from the start: the first record, numbered 1, and where to go on.
    let mut seen = Vec::new();
    let (events, mut next) = poll(&mut harness, client, &observer, 0);
    assert_eq!(names(&events), [("event".to_string(), Some(1))]);
    assert_eq!(events[0]["data"]["kind"], "attempt_started");
    assert_eq!(next, 1);
    seen.extend(events);
    // The engine works; each poll from `next` gives what came since, and nothing twice.
    harness.clock.advance(2_000);
    harness.engine.heartbeat(500);
    harness.engine.log("source", "made 500 rows");
    harness.settle();
    let (events, after) = poll(&mut harness, client, &observer, next);
    assert_eq!(names(&events), [("event".to_string(), Some(2))]);
    assert_eq!(events[0]["data"]["text"], "made 500 rows");
    (next, seen) = (after, [seen, events].concat());
    let (events, after) = poll(&mut harness, client, &observer, next);
    assert!(events.is_empty(), "nothing new");
    assert_eq!(after, next);
    for n in 0..3 {
        harness.engine.log("sink", &format!("counted batch {n}"));
    }
    harness.engine.heartbeat(1000);
    harness.engine.finish(
        r#"{"rows_out":1000,"stages":[{"name":"count","rows":1000}]}"#,
        0,
        None,
    );
    harness.settle();
    // Poll until the exit: the rest of the records, then the report and the exit.
    loop {
        let (events, after) = poll(&mut harness, client, &observer, next);
        let done = events.iter().any(|event| event["event"] == "exit");
        (next, seen) = (after, [seen, events].concat());
        if done {
            break;
        }
    }
    assert_eq!(
        names(&seen),
        [
            ("event".to_string(), Some(1)),
            ("event".to_string(), Some(2)),
            ("event".to_string(), Some(3)),
            ("event".to_string(), Some(4)),
            ("event".to_string(), Some(5)),
            ("report".to_string(), None),
            ("exit".to_string(), None),
        ]
    );
    assert_eq!(next, 5);
    assert_eq!(seen[5]["data"]["report"]["rows_out"], 1000);
    assert_eq!(
        seen[6]["data"],
        json!({ "type": "exit", "code": 0, "diagnostic": null })
    );
    // Read the report.
    let report = tool(
        &mut harness,
        client,
        &observer,
        "moruna_jobs_report",
        json!({ "id": RUN }),
    );
    assert_eq!(
        report["structuredContent"],
        json!({ "rows_out": 1000, "stages": [{ "name": "count", "rows": 1000 }] })
    );
    assert_eq!(report["structuredContent"], seen[5]["data"]["report"]);
    // Resume on a new connection from where an agent left off: the same events from there.
    let again = harness.connect();
    let (events, after) = poll(&mut harness, again, &observer, 3);
    assert_eq!(events, seen[3..]);
    assert_eq!(after, 5);
    // Each poll left one record, as the events' operation.
    let admin = Harness::bearer(&harness.token(&["admin"]));
    let audit = harness.get(client, "/v1/node/audit", &admin).json();
    let polls = audit["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["operation"] == "jobs_events")
        .count();
    assert_eq!(polls, 5);
}

#[test]
fn a_call_answered_after_the_idle_limit_gets_its_answer() {
    // Idle is the client's (system design 2's Limits): a call being answered is not idle, however
    // long its answer takes (a submit waits on a function's fetch); a client silent before its
    // request still is, and the clock starts again from the answer.
    let mut harness = Harness::new();
    let token = harness.token(&["submit"]);
    harness.engine.0.borrow_mut().hold_checks = true;
    let client = harness.connect();
    let silent = harness.connect();
    harness.send(client, &Harness::post("/v1/jobs/validate", &token, JOB));
    harness.settle();
    assert!(harness.clients[client].response().is_none());
    harness.clock.advance(IDLE_LIMIT_MS + 30_000);
    harness.settle();
    assert!(
        !harness.clients[client].server_closed(),
        "a call being answered is not idle"
    );
    assert!(
        harness.clients[silent].server_closed(),
        "a client silent for 60 s before its request is"
    );
    harness.engine.0.borrow_mut().hold_checks = false;
    harness.settle();
    let response = harness.clients[client]
        .response()
        .expect("its answer, 90 s after its request");
    assert_eq!(response.json(), json!({ "ok": true }));
    harness.clock.advance(IDLE_LIMIT_MS - 1);
    harness.settle();
    assert!(!harness.clients[client].server_closed(), "not yet");
    harness.clock.advance(1);
    harness.settle();
    assert!(
        harness.clients[client].server_closed(),
        "idle for 60 s after its answer"
    );
}

#[test]
fn a_stream_with_nothing_to_send_for_60_s_stays_open() {
    // A stream is a request being answered: while its job says nothing, the server is the one to
    // move, and the next event still reaches the client.
    let mut harness = Harness::new();
    submit(&mut harness);
    let stream = follow(&mut harness, "");
    assert_eq!(events(&received(&mut harness, stream)).len(), 1);
    harness.clock.advance(IDLE_LIMIT_MS + 30_000);
    harness.settle();
    assert!(!harness.clients[stream].server_closed());
    harness.engine.log("source", "made 500 rows");
    harness.settle();
    let next = events(&received(&mut harness, stream));
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].2["text"], "made 500 rows");
}

#[test]
fn a_call_whose_client_hangs_up_is_answered_to_nobody() {
    let mut harness = Harness::new();
    let token = harness.token(&["submit"]);
    harness.engine.0.borrow_mut().hold_checks = true;
    let client = harness.connect();
    harness.send(client, &Harness::post("/v1/jobs/validate", &token, JOB));
    harness.settle();
    harness.clients[client].hang_up();
    harness.settle();
    // Its verdict comes to nobody, and the connection goes; the next validate is checked and
    // answered.
    harness.engine.0.borrow_mut().hold_checks = false;
    harness.settle();
    assert!(harness.clients[client].server_closed());
    let token = harness.token(&["submit"]);
    let next = harness.connect();
    let response = harness.exchange(next, &Harness::post("/v1/jobs/validate", &token, JOB));
    assert_eq!(response.json(), json!({ "ok": true }));
    assert_eq!(harness.engine.0.borrow().checked.len(), 2);
}

#[test]
fn a_node_with_no_engine_answers_busy_to_what_needs_one() {
    let mut harness = Harness::without_engine();
    let token = harness.token(&["submit"]);
    for path in ["/v1/jobs/validate", "/v1/jobs"] {
        let client = harness.connect();
        let response = harness.exchange(client, &Harness::post(path, &token, JOB));
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (503, "busy"),
            "{path}"
        );
    }
    let client = harness.connect();
    let observer = Harness::bearer(&harness.token(&["observe"]));
    assert_eq!(
        harness.get(client, "/v1/jobs", &observer).json(),
        json!({ "jobs": [] })
    );
}
