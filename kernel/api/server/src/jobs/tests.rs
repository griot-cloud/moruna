//! The jobs' state against the tests' engine: a job from `spec` to `exit`, the engine's
//! refusals, the checks, the buffer of records and what a stream replays from it, and the
//! secrets that appear in nothing the node keeps.

use super::*;
use crate::fake::{Engine, RUN};
use alloc::string::ToString;
use serde_json::{Value, json};

const NOW: u64 = 1_794_931_200;
const V1: &str = r#"{"v1": {"moruna_spec": 1,
    "source": {"kind": "rows", "rows": 1000},
    "sink": {"kind": "count"}}}"#;

fn body(answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body.bytes()).unwrap()
}

/// The one answer that has come, under `ticket`.
fn resolved(jobs: &mut Jobs, ticket: u64) -> Answer {
    let mut resolved = jobs.resolved();
    assert_eq!(resolved.len(), 1, "one answer");
    let (found, answer) = resolved.remove(0);
    assert_eq!(found, ticket);
    answer
}

/// Everything `stream` can be sent now, as text, and whether it has ended.
fn sent(jobs: &Jobs, stream: &mut Stream) -> (String, bool) {
    let mut all = Vec::new();
    loop {
        // Each fill into an output TLS has emptied, as the session makes it.
        let mut out = Vec::new();
        let ended = jobs.fill(stream, &mut out);
        all.extend_from_slice(&out);
        if ended || out.is_empty() {
            return (String::from_utf8(all).unwrap(), ended);
        }
    }
}

/// A submit as the server makes it: the job queued for the engine's check, then the node pumped
/// once, so that the check answers and a sound job's engine starts. What the node refused at
/// once (the check's refusals, or the start) is the answer, as a submit's answer is.
fn submit(jobs: &mut Jobs, engine: &mut Engine, body: &[u8]) -> Result<u64, Answer> {
    let ticket = jobs.submit(body)?;
    // The first pump hands the engine the check; the next takes its verdict.
    jobs.pump(engine, &Registry::default(), NOW);
    jobs.pump(engine, &Registry::default(), NOW);
    match jobs.resolved.iter().position(|(t, _)| *t == ticket) {
        Some(at) if jobs.resolved[at].1.status != 201 => Err(jobs.resolved.remove(at).1),
        _ => Ok(ticket),
    }
}

/// A job submitted and taken by the engine: its ID is [`RUN`].
fn running(jobs: &mut Jobs, engine: &mut Engine, job: &str) {
    let ticket = submit(jobs, engine, job.as_bytes()).unwrap();
    engine.attempt_started();
    jobs.pump(engine, &Registry::default(), NOW);
    let answer = resolved(jobs, ticket);
    assert_eq!(answer.status, 201);
    assert_eq!(body(&answer), json!({ "id": RUN }));
}

#[test]
fn a_job_goes_from_spec_to_exit() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    // The engine is sent the document as one line, as the client wrote it.
    assert_eq!(
        engine.0.borrow().started,
        [r#"{"type":"spec","spec":{"moruna_spec":1,"source":{"kind":"rows","rows":1000},"sink":{"kind":"count"}}}"#.to_string() + "\n"]
    );
    assert!(jobs.busy());
    // Until the engine tells the run's ID, the submit waits and the node lists no job.
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(jobs.resolved().is_empty() && jobs.list().jobs.is_empty());
    engine.attempt_started();
    jobs.pump(&mut engine, &Registry::default(), NOW + 1);
    assert_eq!(body(&resolved(&mut jobs, ticket)), json!({ "id": RUN }));
    let status = jobs.status(RUN).unwrap();
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({ "id": RUN, "state": "starting", "submitted": NOW })
    );
    // The first heartbeat says it runs.
    engine.heartbeat(100);
    engine.log("source", "made 100 rows");
    jobs.pump(&mut engine, &Registry::default(), NOW + 2);
    let status = jobs.status(RUN).unwrap();
    assert_eq!(
        (status.state, status.started),
        (JobState::Running, Some(NOW + 2))
    );
    let heartbeat = status.heartbeat.unwrap();
    assert_eq!(
        (heartbeat.rows_out, heartbeat.bytes_out, heartbeat.t_ms),
        (100, 800, 1000)
    );
    assert_eq!(heartbeat.bottleneck.as_deref(), Some("source"));
    assert_eq!(jobs.report(RUN).status, 409, "not before it has ended");
    engine.heartbeat(1000);
    engine.finish(r#"{"rows_out": 1000, "exit": {"code": 0}}"#, 0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW + 9);
    assert!(!jobs.busy());
    let status = jobs.status(RUN).unwrap();
    assert_eq!(
        (status.state, status.ended, status.exit_code),
        (JobState::Completed, Some(NOW + 9), Some(0))
    );
    assert_eq!(status.heartbeat.unwrap().rows_out, 1000);
    // The report is the engine's, as it wrote it.
    let report = jobs.report(RUN);
    assert_eq!(report.status, 200);
    assert_eq!(
        report.body.bytes(),
        br#"{"rows_out": 1000, "exit": {"code": 0}}"#
    );
    assert_eq!(
        serde_json::to_value(jobs.list()).unwrap(),
        json!({ "jobs": [{ "id": RUN, "state": "completed", "submitted": NOW,
                           "started": NOW + 2, "ended": NOW + 9 }] })
    );
    assert_eq!(jobs.buffered().1, 2);
}

#[test]
fn a_version_2_document_goes_to_the_engine_as_its_base64() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    submit(&mut jobs, &mut engine, br#"{"v2":"TUpCMg=="}"#).unwrap();
    assert_eq!(engine.spec(), json!({ "type": "spec", "spec": "TUpCMg==" }));
    // The check reads its bytes.
    let mut checks = Jobs::default();
    checks.validate(br#"{"v2":"TUpCMg=="}"#).unwrap();
    checks.validate(V1.as_bytes()).unwrap();
    checks.pump(&mut engine, &Registry::default(), NOW);
    checks.pump(&mut engine, &Registry::default(), NOW);
    checks.pump(&mut engine, &Registry::default(), NOW);
    // The submit's own check first, then the two validations'.
    let checked = engine.0.borrow().checked.clone();
    assert_eq!(checked[0], b"MJB2");
    assert_eq!(checked[1], b"MJB2");
    assert!(checked[2].starts_with(br#"{"moruna_spec":1,"#));
}

#[test]
fn what_is_not_a_job_is_invalid_and_starts_nothing() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    for bad in [
        "",
        "[]",
        "{}",
        r#"{"v1":{},"v2":"TUpCMg=="}"#,
        r#"{"v1":[1]}"#,
        r#"{"v1":"{}"}"#,
        r#"{"v2":"not base64"}"#,
        r#"{"v2":""}"#,
        r#"{"v1":{},"budget":1}"#,
        r#"{"v1":{},"secrets":[""]}"#,
        r#"{"v1":{},"secrets":"s"}"#,
        r#"{"v1":{},"secrets":{"key":""}}"#,
        r#"{"v1":{},"secrets":{"a key":"v"}}"#,
        r#"{"v1":{},"secrets":{"k":7}}"#,
    ] {
        let answer = submit(&mut jobs, &mut engine, bad.as_bytes()).unwrap_err();
        assert_eq!(answer.status, 400, "{bad}");
        assert_eq!(body(&answer)["error"]["code"], "invalid", "{bad}");
        assert_eq!(
            jobs.validate(bad.as_bytes()).unwrap_err().status,
            400,
            "{bad}"
        );
    }
    assert!(engine.0.borrow().started.is_empty() && !jobs.busy());
}

#[test]
fn the_engine_refusal_answers_the_submit_and_leaves_no_job() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    // Exit code 2, with the refusal: invalid, with the engine's words.
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    engine.finish(
        r#"{"exit":{"code":2}}"#,
        2,
        Some("source.kind: no source kind is named `rows`"),
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let answer = resolved(&mut jobs, ticket);
    assert_eq!(answer.status, 400);
    assert_eq!(
        body(&answer),
        json!({ "error": { "code": "invalid",
                           "message": "source.kind: no source kind is named `rows`" } })
    );
    assert!(!jobs.busy() && jobs.list().jobs.is_empty());
    // Any other end before the job began is the node's failure.
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    engine.exit(3, Some("the budget does not fit"));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let answer = resolved(&mut jobs, ticket);
    assert_eq!(answer.status, 500);
    assert!(
        body(&answer)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("code 3")
    );
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    engine.0.borrow_mut().ended = true;
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(resolved(&mut jobs, ticket).status, 500);
    assert!(!jobs.busy());
}

#[test]
fn a_submit_while_a_job_runs_is_busy_and_the_host_can_refuse_one() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    let answer = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap_err();
    assert_eq!(
        (answer.status, body(&answer)["error"]["code"].as_str()),
        (503, Some("busy"))
    );
    assert_eq!(engine.0.borrow().started.len(), 1);
    engine.exit(0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    for (refusal, status) in [
        (
            HostError::Refused("the budget is past what the node gives a job".into()),
            400,
        ),
        (HostError::Failed("node-admission is gone".into()), 500),
        (HostError::Unavailable, 503),
        (HostError::WouldBlock, 503),
    ] {
        engine.0.borrow_mut().refuse_start = Some(refusal);
        let answer = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap_err();
        assert_eq!(answer.status, status);
        assert!(!jobs.busy());
    }
}

#[test]
fn cancel_tells_the_engine_and_a_cancelled_job_says_so() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    assert_eq!(jobs.cancel(&mut engine, RUN).status, 404);
    running(&mut jobs, &mut engine, V1);
    let answer = jobs.cancel(&mut engine, RUN);
    assert_eq!(answer.status, 200);
    assert_eq!(body(&answer)["state"], "starting", "the job as it stands");
    assert_eq!(engine.0.borrow().sent, ["{\"type\":\"cancel\"}\n"]);
    engine.exit(130, Some("cancelled"));
    jobs.pump(&mut engine, &Registry::default(), NOW + 3);
    let status = jobs.status(RUN).unwrap();
    assert_eq!(
        (status.state, status.exit_code),
        (JobState::Cancelled, Some(130))
    );
    let answer = jobs.cancel(&mut engine, RUN);
    assert_eq!(
        (answer.status, body(&answer)["error"]["code"].as_str()),
        (409, Some("conflict"))
    );
    assert_eq!(engine.0.borrow().sent.len(), 1);
    assert_eq!(jobs.report(RUN).status, 404, "it ended with no report");
    assert_eq!(jobs.report("nothing").status, 404);
    assert!(jobs.status("nothing").is_none() && jobs.stream("nothing", None).is_none());
}

#[test]
fn a_job_ends_failed_on_any_other_code_or_when_the_engine_is_lost() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    engine.exit(4, Some("kernel error"));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let status = jobs.status(RUN).unwrap();
    assert_eq!(
        (status.state, status.exit_code),
        (JobState::Failed, Some(4))
    );
    // The channel ends with no exit.
    engine.run_as("11111111111111111111111111111111");
    running_as(&mut jobs, &mut engine, "11111111111111111111111111111111");
    engine.0.borrow_mut().ended = true;
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let status = jobs.status("11111111111111111111111111111111").unwrap();
    assert_eq!((status.state, status.exit_code), (JobState::Failed, None));
    // Bytes that are not the protocol's.
    engine.run_as("22222222222222222222222222222222");
    running_as(&mut jobs, &mut engine, "22222222222222222222222222222222");
    engine.bytes(&[0, 1, 0, 0, 0]);
    engine.heartbeat(5);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let status = jobs.status("22222222222222222222222222222222").unwrap();
    assert_eq!(status.state, JobState::Failed);
    assert!(status.heartbeat.is_none() && !jobs.busy());
    assert_eq!(jobs.list().jobs.len(), 3);
}

/// As [`running`], for a job whose ID is `run`.
fn running_as(jobs: &mut Jobs, engine: &mut Engine, run: &str) {
    let ticket = submit(jobs, engine, V1.as_bytes()).unwrap();
    engine.attempt_started();
    jobs.pump(engine, &Registry::default(), NOW);
    assert_eq!(body(&resolved(jobs, ticket)), json!({ "id": run }));
}

#[test]
fn what_the_engine_says_before_the_run_id_is_kept_for_the_job() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    engine.heartbeat(7);
    engine.say(r#"{"type":"report","report":{"early":true},"plan":null}"#);
    engine.say("not json");
    engine.say(r#"{"type":"limits_changed","reason":"memory"}"#);
    engine.bytes(b"\xff\xfe\n");
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(jobs.resolved().is_empty());
    engine.attempt_started();
    jobs.pump(&mut engine, &Registry::default(), NOW + 1);
    assert_eq!(resolved(&mut jobs, ticket).status, 201);
    let status = jobs.status(RUN).unwrap();
    assert_eq!(status.state, JobState::Running);
    assert_eq!(status.heartbeat.unwrap().rows_out, 7);
    engine.exit(0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW + 2);
    assert_eq!(jobs.report(RUN).body.bytes(), br#"{"early":true}"#);
}

#[test]
fn checks_run_one_at_a_time_in_order() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    engine.0.borrow_mut().verdicts.extend([
        Ok(vec![Refusal {
            field: "source.kind".into(),
            message: "no source kind is named `rows`".into(),
        }]),
        Ok(Vec::new()),
        Err(HostError::Failed("the check process died".into())),
    ]);
    engine.0.borrow_mut().hold_checks = true;
    let first = jobs.validate(V1.as_bytes()).unwrap();
    let second = jobs.validate(br#"{"v1":{"n":2}}"#).unwrap();
    let third = jobs.validate(br#"{"v1":{"n":3}}"#).unwrap();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(engine.0.borrow().checked.len(), 1, "one at a time");
    assert!(jobs.resolved().is_empty());
    engine.0.borrow_mut().hold_checks = false;
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let answer = resolved(&mut jobs, first);
    assert_eq!(answer.status, 200);
    assert_eq!(
        body(&answer),
        json!({ "ok": false, "refusals": [{ "field": "source.kind",
                                             "message": "no source kind is named `rows`" }] })
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(body(&resolved(&mut jobs, second)), json!({ "ok": true }));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(resolved(&mut jobs, third).status, 500);
    assert_eq!(engine.0.borrow().checked[1], br#"{"n":2}"#);
    // A check also runs while a job does.
    running(&mut jobs, &mut engine, V1);
    let beside = jobs.validate(V1.as_bytes()).unwrap();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(body(&resolved(&mut jobs, beside)), json!({ "ok": true }));
}

#[test]
fn an_answer_nobody_waits_for_is_let_go() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    engine.0.borrow_mut().hold_checks = true;
    let first = jobs.validate(V1.as_bytes()).unwrap();
    let second = jobs.validate(V1.as_bytes()).unwrap();
    let third = jobs.validate(V1.as_bytes()).unwrap();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    // The one being checked, and one still waiting.
    jobs.abandon(first);
    jobs.abandon(second);
    engine.0.borrow_mut().hold_checks = false;
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(
        jobs.resolved().is_empty(),
        "the first verdict is not wanted"
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(resolved(&mut jobs, third).status, 200);
    assert_eq!(
        engine.0.borrow().checked.len(),
        2,
        "the second was never checked"
    );
    // An answer that had come, and a submit's.
    let late = jobs.validate(V1.as_bytes()).unwrap();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    jobs.abandon(late);
    assert!(jobs.resolved().is_empty());
    let submit = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    jobs.abandon(submit);
    engine.attempt_started();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(jobs.resolved().is_empty());
    assert_eq!(
        jobs.status(RUN).unwrap().state,
        JobState::Starting,
        "the job goes on"
    );
    // A host that cannot check now is asked again; one that never can says so.
    let mut none = crate::NoEngine;
    let ticket = Jobs::default().validate(V1.as_bytes()).map(|ticket| {
        let mut jobs = Jobs::default();
        let waiting = jobs.validate(V1.as_bytes()).unwrap();
        jobs.pump(&mut none, &Registry::default(), NOW);
        (ticket, resolved(&mut jobs, waiting).status)
    });
    assert_eq!(ticket.unwrap().1, 503);
}

#[test]
fn a_stream_sends_records_heartbeats_then_the_report_and_the_exit() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    engine.log("source", "one");
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let mut stream = jobs.stream(RUN, None).unwrap();
    let (text, ended) = sent(&jobs, &mut stream);
    assert!(!ended);
    let events: Vec<&str> = text.split("\n\n").filter(|e| !e.is_empty()).collect();
    assert_eq!(events.len(), 2, "{text}");
    assert!(events[0].starts_with("id: 1\nevent: event\ndata: {\"type\":\"event\",\"v\":7,"));
    assert!(events[0].contains("\"kind\":\"attempt_started\""));
    assert!(events[1].starts_with("id: 2\nevent: event\ndata: {"));
    assert!(events[1].contains("\"kind\":\"log\"") && events[1].contains("\"text\":\"one\""));
    for event in &events {
        let data = event.rsplit("data: ").next().unwrap();
        assert!(!data.contains('\n'));
        let _: Value = serde_json::from_str(data).expect("each event's data is one JSON line");
    }
    // Nothing new: nothing sent. Then a heartbeat, and a record.
    assert_eq!(sent(&jobs, &mut stream), (String::new(), false));
    engine.heartbeat(10);
    engine.heartbeat(20);
    engine.log("source", "two");
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let (text, _) = sent(&jobs, &mut stream);
    let events: Vec<&str> = text.split("\n\n").filter(|e| !e.is_empty()).collect();
    assert_eq!(
        events.len(),
        2,
        "the record, then the latest heartbeat: {text}"
    );
    assert!(events[0].starts_with("id: 3\nevent: event\n"));
    assert!(events[1].starts_with("event: heartbeat\ndata: {\"type\":\"heartbeat\""));
    assert!(events[1].contains("\"rows_out\":20"));
    // The end: the report, then the exit, and the stream is over.
    engine.finish(r#"{"rows_out":20}"#, 0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let (text, ended) = sent(&jobs, &mut stream);
    assert!(ended);
    assert_eq!(
        text,
        "event: report\ndata: {\"type\":\"report\",\"report\":{\"rows_out\":20},\"plan\":{\"id\":\"plan-1\"}}\n\n\
         event: exit\ndata: {\"type\":\"exit\",\"code\":0,\"diagnostic\":null}\n\n"
    );
    // A replay of the ended job after its second record: from its third on.
    let mut replay = jobs.stream(RUN, Some(2)).unwrap();
    let (text, ended) = sent(&jobs, &mut replay);
    assert!(ended);
    assert!(text.starts_with("id: 3\nevent: event\n"), "{text}");
    assert!(
        !text.contains("event: heartbeat"),
        "heartbeats are not replayed"
    );
    assert!(text.ends_with("\"diagnostic\":null}\n\n") && text.contains("event: report\n"));
    // After its last record, or past it: only the end. After zero: all of it.
    for from in [3, 99] {
        let (text, _) = sent(&jobs, &mut jobs.stream(RUN, Some(from)).unwrap());
        assert!(text.starts_with("event: report\n"), "{from}: {text}");
    }
    let (text, _) = sent(&jobs, &mut jobs.stream(RUN, Some(0)).unwrap());
    assert!(text.starts_with("id: 1\n"));
}

/// The page of the events' tool for [`RUN`] after its record `after`, parsed.
fn page(jobs: &Jobs, after: u64) -> Value {
    serde_json::from_str(&jobs.page(RUN, after).expect("the job is kept")).expect("a page is JSON")
}

/// Each event of `page`: its name, and its number when it has one.
fn shape(page: &Value) -> Vec<(String, Option<u64>)> {
    page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            (
                event["event"].as_str().unwrap().to_string(),
                event["id"].as_u64(),
            )
        })
        .collect()
}

/// `(name, id)` for a record numbered `id`.
fn record(id: u64) -> (String, Option<u64>) {
    ("event".to_string(), Some(id))
}

/// `(name, None)` for an event with no number.
fn named(name: &str) -> (String, Option<u64>) {
    (name.to_string(), None)
}

#[test]
fn a_page_holds_the_records_after_from_then_the_end_and_where_to_go_on() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    // Running, with its first record: that record; after it, nothing yet.
    let first = page(&jobs, 0);
    assert_eq!(shape(&first), [record(1)]);
    assert_eq!(first["records"][0]["data"]["kind"], "attempt_started");
    assert_eq!(first["next"], 1);
    assert_eq!(page(&jobs, 1), json!({ "records": [], "next": 1 }));
    // A heartbeat and a record come. A page holds no heartbeat, as a fresh stream sends none
    // until the next one comes: the job's latest is `jobs_get`'s.
    engine.heartbeat(10);
    engine.log("source", "two");
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let second = page(&jobs, 1);
    assert_eq!(shape(&second), [record(2)]);
    assert_eq!(second["records"][0]["data"]["text"], "two");
    assert_eq!(second["next"], 2);
    // It ends: after the last record, the report and the exit, with no number; the page says
    // to go on from where it was, and polled again it says the same.
    engine.finish(r#"{"rows_out":20}"#, 0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let end = page(&jobs, 2);
    assert_eq!(shape(&end), [named("report"), named("exit")]);
    assert_eq!(
        end["records"][0]["data"],
        json!({ "type": "report", "report": { "rows_out": 20 }, "plan": { "id": "plan-1" } })
    );
    assert_eq!(
        end["records"][1]["data"],
        json!({ "type": "exit", "code": 0, "diagnostic": null })
    );
    assert_eq!(end["next"], 2);
    assert_eq!(page(&jobs, 2), end);
    // From the start, the page is what a fresh stream sends, event for event.
    let whole = page(&jobs, 0);
    let (text, ended) = sent(&jobs, &mut jobs.stream(RUN, None).unwrap());
    assert!(ended);
    let streamed: Vec<Value> = text
        .split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| {
            let data = event.rsplit("data: ").next().unwrap();
            serde_json::from_str(data).unwrap()
        })
        .collect();
    let paged: Vec<Value> = whole["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["data"].clone())
        .collect();
    assert_eq!(paged, streamed);
    assert_eq!(
        shape(&whole),
        [record(1), record(2), named("report"), named("exit")]
    );
    assert!(jobs.page("ffffffffffffffffffffffffffffffff", 0).is_none());
}

#[test]
fn a_page_holds_at_most_1_000_events_and_its_end_whole() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    for n in 2..=1_500 {
        engine.log("source", &format!("line {n}"));
    }
    // One line too long to read: the end will carry a note of it.
    engine.say(&"x".repeat(ENGINE_LINE_LIMIT + 1));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let first = page(&jobs, 0);
    let ids: Vec<u64> = shape(&first).iter().map(|event| event.1.unwrap()).collect();
    assert_eq!(ids, (1..=1_000).collect::<Vec<u64>>());
    assert_eq!(first["next"], 1_000);
    // Caught up while the job runs: the rest, then the note, as a fresh stream sends it.
    let rest = page(&jobs, 1_000);
    let events = shape(&rest);
    assert_eq!(events.len(), 501);
    assert_eq!(events[499], record(1_500));
    assert_eq!(events[500], named("note"));
    assert_eq!(rest["records"][500]["data"]["skipped_lines"], 1);
    assert_eq!(rest["next"], 1_500);
    engine.finish("{}", 0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    // The end is three events: with 997 records it fits, with 998 it waits for the next page.
    let fits = shape(&page(&jobs, 503));
    assert_eq!(fits.len(), 1_000);
    assert_eq!(fits[996], record(1_500));
    assert_eq!(fits[997..], [named("note"), named("report"), named("exit")]);
    let waits = page(&jobs, 502);
    assert_eq!(shape(&waits).len(), 998);
    assert_eq!(waits["next"], 1_500);
    assert_eq!(
        shape(&page(&jobs, 1_500)),
        [named("note"), named("report"), named("exit")]
    );
    // 1,000 records exactly leave no room for the end.
    let full = page(&jobs, 500);
    assert_eq!(shape(&full).len(), 1_000);
    assert_eq!(full["next"], 1_500);
}

/// The bytes of the events' JSON in `page`.
fn data_bytes(page: &Value) -> usize {
    page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["data"].to_string().len())
        .sum()
}

#[test]
fn a_page_holds_at_most_1_mib_of_events_but_always_one() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    // Records of a little over 100 KiB: after the first, small, record, ten fit in 1 MiB and
    // the eleventh does not.
    let line = "x".repeat(100 * 1024);
    for _ in 0..20 {
        engine.log("source", &line);
    }
    // A record whose JSON alone is over 1 MiB: each byte under 0x20 is written `\u00XX`.
    engine.log("source", &"\u{1}".repeat(200 * 1024));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let ids = |page: &Value| -> Vec<u64> {
        shape(page)
            .iter()
            .map(|event| event.1.expect("a record"))
            .collect()
    };
    let first = page(&jobs, 0);
    assert_eq!(ids(&first), (1..=11).collect::<Vec<u64>>());
    assert!(data_bytes(&first) <= EVENTS_PAGE_BYTES);
    assert_eq!(first["next"], 11);
    let second = page(&jobs, 11);
    assert_eq!(ids(&second), (12..=21).collect::<Vec<u64>>());
    assert_eq!(second["next"], 21);
    // The large record has a page of its own.
    let large = page(&jobs, 21);
    assert_eq!(ids(&large), [22]);
    assert!(data_bytes(&large) > EVENTS_PAGE_BYTES);
    assert_eq!(large["next"], 22);
    // The end, with a report of 950 KiB, does not fit after it: it comes alone, next.
    engine.finish(
        &format!("{{\"pad\":\"{}\"}}", "r".repeat(950 * 1024)),
        0,
        None,
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(ids(&page(&jobs, 21)), [22]);
    let end = page(&jobs, 22);
    assert_eq!(shape(&end), [named("report"), named("exit")]);
    assert_eq!(end["next"], 22);
    // After record 20: the record of 100 KiB, which the large one does not fit after.
    assert_eq!(shape(&page(&jobs, 20)), [record(21)]);
}

#[test]
fn a_page_passes_over_what_the_buffer_has_let_go() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    let line = "x".repeat(512 * 1024);
    for _ in 0..40 {
        engine.log("source", &line);
        jobs.pump(&mut engine, &Registry::default(), NOW);
    }
    // Followed from the start, the pages begin at the oldest record kept, and their numbers show
    // the gap. Records of 512 KiB come one to a page: two are over 1 MiB.
    let records = jobs.buffered().1 as u64;
    let (mut ids, mut next) = (Vec::new(), 0);
    while next < 41 {
        let kept = page(&jobs, next);
        assert_eq!(shape(&kept).len(), 1);
        ids.extend(shape(&kept).iter().map(|event| event.1.unwrap()));
        next = kept["next"].as_u64().unwrap();
    }
    let first = 41 - records + 1;
    assert_eq!(
        ids,
        (first..=41).collect::<Vec<u64>>(),
        "the numbers show the gap"
    );
    // A job whose records have all gone gives the page its end alone.
    engine.finish("{}", 0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    engine.run_as("11111111111111111111111111111111");
    running_as(&mut jobs, &mut engine, "11111111111111111111111111111111");
    for _ in 0..40 {
        engine.log("source", &line);
        jobs.pump(&mut engine, &Registry::default(), NOW);
    }
    assert_eq!(
        shape(&page(&jobs, 0)),
        [named("report"), named("exit")],
        "the first job's records are all gone"
    );
    assert_eq!(page(&jobs, 7)["next"], 7);
}

#[test]
fn the_buffer_keeps_the_last_records_within_its_bytes_and_a_stream_passes_the_gap() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    let mut slow = jobs.stream(RUN, None).unwrap();
    // Records of about 512 KiB each: the buffer's bytes run out long before its count does.
    let line = "x".repeat(512 * 1024);
    for _ in 0..40 {
        engine.log("source", &line);
        jobs.pump(&mut engine, &Registry::default(), NOW);
    }
    let (bytes, records) = jobs.buffered();
    assert!(bytes <= EVENT_BUFFER_BYTES && bytes > EVENT_BUFFER_BYTES - 1024 * 1024);
    assert!((20..41).contains(&records), "{records}");
    // The slow stream goes on from the oldest record still kept, and its IDs show the gap.
    let mut out = Vec::new();
    assert!(!jobs.fill(&mut slow, &mut out));
    let text = String::from_utf8(out).unwrap();
    let first = 41 - records as u64 + 1;
    assert!(
        text.starts_with(&format!("id: {first}\nevent: event\n")),
        "{}",
        &text[..40]
    );
    // A batch is bounded: one record of this size at a time.
    assert_eq!(text.matches("\nevent: event\n").count(), 1);
}

#[test]
fn the_buffer_keeps_at_most_100_000_records() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    for batch in 0..11 {
        for _ in 0..10_000 {
            engine.event(&moruna_event::Event::AttemptStarted {
                resumed: false,
                plan: None,
            });
        }
        jobs.pump(&mut engine, &Registry::default(), NOW);
        assert!(jobs.buffered().1 <= EVENTS_KEPT, "{batch}");
    }
    let (bytes, records) = jobs.buffered();
    assert!(
        records == EVENTS_KEPT || bytes > EVENT_BUFFER_BYTES - 4096,
        "{records} {bytes}"
    );
    assert_eq!(jobs.status(RUN).unwrap().state, JobState::Starting);
}

#[test]
fn the_last_100_jobs_are_kept_and_their_reports_within_their_bytes() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    let report = format!("{{\"pad\":\"{}\"}}", "r".repeat(600 * 1024));
    for n in 0..JOBS_KEPT + 2 {
        let run = format!("{n:032x}");
        engine.run_as(&run);
        running_as(&mut jobs, &mut engine, &run);
        engine.finish(&report, 0, None);
        jobs.pump(&mut engine, &Registry::default(), NOW + n as u64);
    }
    let list = jobs.list().jobs;
    assert_eq!(list.len(), JOBS_KEPT);
    assert_eq!(
        list[0].id,
        format!("{:032x}", JOBS_KEPT + 1),
        "newest first"
    );
    assert_eq!(
        list[JOBS_KEPT - 1].id,
        format!("{:032x}", 2),
        "the oldest two are gone"
    );
    assert!(jobs.status(&format!("{:032x}", 0)).is_none());
    // Three reports of 600 KiB fit in 2 MiB; the fourth newest job's is gone, its job is not.
    for (back, status) in [(0, 200), (1, 200), (2, 200), (3, 404), (50, 404)] {
        let run = format!("{:032x}", JOBS_KEPT + 1 - back);
        assert_eq!(jobs.report(&run).status, status, "{back}");
        assert_eq!(jobs.status(&run).unwrap().state, JobState::Completed);
    }
    // A report past what all may take is not kept at all.
    engine.run_as(RUN);
    running(&mut jobs, &mut engine, V1);
    // The engine's line limit is under the reports' bytes, so such a report is a line passed
    // over: the job ends with none, and a stream of its events says one line was skipped.
    engine.finish(
        &format!("{{\"pad\":\"{}\"}}", "r".repeat(ENGINE_LINE_LIMIT)),
        0,
        None,
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(jobs.status(RUN).unwrap().state, JobState::Completed);
    assert_eq!(jobs.report(RUN).status, 404);
    let (text, ended) = sent(&jobs, &mut jobs.stream(RUN, None).unwrap());
    assert!(ended);
    let note = text
        .split("\n\n")
        .find(|event| event.starts_with("event: note\n"))
        .expect("a note");
    let data: Value =
        serde_json::from_str(note.strip_prefix("event: note\ndata: ").unwrap()).unwrap();
    assert_eq!(
        (&data["type"], &data["skipped_lines"]),
        (&json!("note"), &json!(1))
    );
    assert!(data["message"].as_str().unwrap().contains("skipped"));
}

#[test]
fn what_follows_the_records_goes_a_batch_at_a_time_and_the_stream_goes_on() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    running(&mut jobs, &mut engine, V1);
    let mut stream = jobs.stream(RUN, Some(1)).unwrap();
    // A heartbeat larger than a batch: the stream sends it over two fills.
    let bottleneck = "b".repeat(40 * 1024);
    engine.say(&format!(
        r#"{{"type":"heartbeat","t_ms":1,"rows_out":1,"bottleneck":"{bottleneck}"}}"#
    ));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    // Each fill as the session makes it: into an output TLS has emptied.
    let (mut first, mut second) = (Vec::new(), Vec::new());
    assert!(!jobs.fill(&mut stream, &mut first));
    assert_eq!(first.len(), STREAM_BATCH);
    assert!(!jobs.fill(&mut stream, &mut second));
    first.extend_from_slice(&second);
    let text = String::from_utf8(first).unwrap();
    assert!(text.starts_with("event: heartbeat\ndata: {"));
    assert!(text.ends_with(&format!("{bottleneck}\"}}\n\n")));
    // It has not ended: the next record follows, and then the end, with a report that has no
    // plan.
    engine.log("source", "after");
    engine.say(r#"{"type":"report","report":{"rows_out":1}}"#);
    engine.exit(0, None);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let (text, ended) = sent(&jobs, &mut stream);
    assert!(ended);
    assert!(text.starts_with("id: 2\nevent: event\n"), "{text}");
    assert!(text.contains(
        "event: report\ndata: {\"type\":\"report\",\"report\":{\"rows_out\":1},\"plan\":null}\n\n"
    ));
    assert!(text.ends_with("\"diagnostic\":null}\n\n"));
}

#[test]
fn a_line_skipped_before_the_run_id_is_counted_for_the_job() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    let ticket = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap();
    engine.say(&"x".repeat(ENGINE_LINE_LIMIT + 1));
    engine.say(&"y".repeat(ENGINE_LINE_LIMIT + 1));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    engine.attempt_started();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert_eq!(resolved(&mut jobs, ticket).status, 201);
    let mut stream = jobs.stream(RUN, None).unwrap();
    let (text, _) = sent(&jobs, &mut stream);
    assert!(text.contains("\"skipped_lines\":2"), "{text}");
    // Told once; told again when more are skipped.
    assert_eq!(sent(&jobs, &mut stream).0, "");
    engine.say(&"z".repeat(ENGINE_LINE_LIMIT + 1));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(sent(&jobs, &mut stream).0.contains("\"skipped_lines\":3"));
}

#[test]
fn a_secret_appears_in_nothing_the_node_keeps_or_answers() {
    const SECRET: &str = "AKIA/very+secret=key";
    let job = format!(
        r#"{{"v1":{{"moruna_spec":1,"object_store":{{"s3":{{"secret_access_key":{{"secret":"s3.key"}}}}}}}},"secrets":{{"s3.key":"{SECRET}","other":"unused"}}}}"#
    );
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    // The engine has it, beside the document and never in it.
    running(&mut jobs, &mut engine, &job);
    let spec = engine.spec();
    assert_eq!(
        spec["secrets"],
        json!({ "s3.key": SECRET, "other": "unused" })
    );
    assert_eq!(
        spec["spec"]["object_store"]["s3"]["secret_access_key"],
        json!({ "secret": "s3.key" })
    );
    assert!(!spec["spec"].to_string().contains(SECRET));
    // And it leaks it every way it can.
    engine.log("source", &format!("opening with key {SECRET}"));
    engine.say(&format!(
        r#"{{"type":"heartbeat","t_ms":1,"rows_out":1,"bottleneck":"{SECRET}"}}"#
    ));
    engine.finish(
        &format!(r#"{{"source":{{"key":"{SECRET}"}}}}"#),
        4,
        Some(&format!("source failed with key {SECRET}")),
    );
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let mut all = String::new();
    let (events, ended) = sent(&jobs, &mut jobs.stream(RUN, None).unwrap());
    assert!(ended);
    all.push_str(&events);
    all.push_str(&String::from_utf8(jobs.report(RUN).body.bytes()).unwrap());
    all.push_str(&serde_json::to_string(&jobs.status(RUN).unwrap()).unwrap());
    all.push_str(&serde_json::to_string(&jobs.list()).unwrap());
    assert!(!all.contains(SECRET), "{all}");
    assert_eq!(all.matches("[redacted]").count(), 5, "{all}");
    // A page of the events' tool: the log, the report and the exit, each redacted.
    let page = jobs.page(RUN, 0).unwrap();
    assert!(!page.contains(SECRET), "{page}");
    assert_eq!(page.matches("[redacted]").count(), 3, "{page}");
    assert_eq!(
        jobs.status(RUN)
            .unwrap()
            .heartbeat
            .unwrap()
            .bottleneck
            .as_deref(),
        Some("[redacted]")
    );
    // The engine's refusal of a submit, and of a check.
    let ticket = submit(&mut jobs, &mut engine, job.as_bytes()).unwrap();
    engine.exit(2, Some(&format!("source.key: {SECRET} is not a key")));
    jobs.pump(&mut engine, &Registry::default(), NOW);
    let refused = String::from_utf8(resolved(&mut jobs, ticket).body.bytes()).unwrap();
    assert!(
        !refused.contains(SECRET) && refused.contains("[redacted] is not a key"),
        "{refused}"
    );
    engine.0.borrow_mut().verdicts.push_back(Ok(vec![Refusal {
        field: "source.key".into(),
        message: format!("{SECRET} is not a key"),
    }]));
    let ticket = jobs.validate(job.as_bytes()).unwrap();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    jobs.pump(&mut engine, &Registry::default(), NOW);
    // The check is told the secrets' names, and never their values.
    let names = engine.0.borrow().checked_names.last().unwrap().clone();
    assert_eq!(names, ["other", "s3.key"]);
    let refused = String::from_utf8(resolved(&mut jobs, ticket).body.bytes()).unwrap();
    assert!(
        !refused.contains(SECRET) && refused.contains("[redacted] is not a key"),
        "{refused}"
    );
    // The host's own refusal.
    engine.0.borrow_mut().refuse_start = Some(HostError::Refused(format!("no volume {SECRET}")));
    let refused = submit(&mut jobs, &mut engine, job.as_bytes()).unwrap_err();
    assert!(
        !String::from_utf8(refused.body.bytes())
            .unwrap()
            .contains(SECRET)
    );
}

/// A submit is the engine's check first (system design 3A): a refused document answers
/// `invalid` with each refusal by its field and starts nothing; a sound one starts an engine
/// with the check's budget and needs; the node is busy from the submit on; and a submit whose
/// call has gone still becomes the job.
#[test]
fn a_submit_is_checked_and_started_with_the_checks_budget_and_needs() {
    let (mut jobs, mut engine) = (Jobs::default(), Engine::new());
    engine.0.borrow_mut().verdicts.push_back(Ok(vec![Refusal {
        field: "budget.memory_bytes".into(),
        message: "more than a job on this node may have".into(),
    }]));
    let refused = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap_err();
    assert_eq!(
        (refused.status, body(&refused)["error"]["code"].as_str()),
        (400, Some("invalid"))
    );
    assert!(
        body(&refused)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("budget.memory_bytes: more than a job on this node may have"),
        "{}",
        body(&refused)
    );
    assert!(engine.0.borrow().started.is_empty() && !jobs.busy());

    let needs = crate::Needs {
        hosts: vec!["s3.eu-west-1.amazonaws.com".into()],
        volumes: vec![crate::Volume {
            serial: "vol-out".into(),
            write: true,
        }],
        functions: vec!["taxi.clean".into()],
    };
    engine.0.borrow_mut().needs = (Some(1 << 30), needs.clone());
    // A function the job uses that is not registered refuses the submit too (3E).
    let refused = submit(&mut jobs, &mut engine, V1.as_bytes()).unwrap_err();
    assert!(
        body(&refused)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("functions: the function `taxi.clean` is not registered"),
        "{}",
        body(&refused)
    );
    assert!(engine.0.borrow().started.is_empty() && !jobs.busy());

    let signer = crate::signer::Signer::new();
    let mut registry = Registry::new(vec![crate::FunctionKey::new(signer.public())]);
    let image = b"\x7fELF taxi.clean";
    let address = "https://images.example.com/taxi.clean";
    let registration = crate::signer::registration("taxi.clean", address, image, &signer);
    assert_eq!(registry.register(registration.as_bytes(), NOW).status, 201);
    engine.0.borrow_mut().hold_checks = true;
    let ticket = jobs.submit(V1.as_bytes()).unwrap();
    jobs.pump(&mut engine, &registry, NOW);
    // Being checked, the job is starting: a second submit is busy.
    assert!(jobs.busy());
    let busy = jobs.submit(V1.as_bytes()).unwrap_err();
    assert_eq!(busy.status, 503);
    // The call goes; the job goes on.
    jobs.abandon(ticket);
    engine.0.borrow_mut().hold_checks = false;
    jobs.pump(&mut engine, &registry, NOW);
    let started = engine.0.borrow().started_with.clone();
    assert_eq!(
        started,
        [Verdict {
            refusals: Vec::new(),
            memory_bytes: Some(1 << 30),
            needs,
        }]
    );
    engine.attempt_started();
    jobs.pump(&mut engine, &Registry::default(), NOW);
    assert!(jobs.resolved().is_empty(), "nobody waits for its id");
    assert_eq!(
        serde_json::to_value(jobs.list()).unwrap()["jobs"][0]["id"],
        json!(RUN)
    );
}
