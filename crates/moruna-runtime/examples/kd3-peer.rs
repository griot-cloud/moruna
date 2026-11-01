//! Actual KD2/KD3 socket peer over canonical events7 framing.
#![deny(unsafe_code)]
#[path = "kd3/common.rs"]
mod common;
use common::{PATHS, Result, cases, checked_job};
use moruna_runtime::host::protocol::{self, Inbound, Outbound};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};
fn connect(path: &Path, port: u32) -> Result<BufReader<UnixStream>> {
    Ok(moruna_vmm::session::connect_guest(
        path,
        port,
        Instant::now() + Duration::from_secs(1800),
        &|| path.exists(),
    )?)
}
fn send(conn: &mut UnixStream, message: &Inbound) -> Result<()> {
    serde_json::to_writer(&mut *conn, message)?;
    conn.write_all(b"\n")?;
    Ok(())
}
fn converse(mut reader: BufReader<UnixStream>, spec: Value, cancel: bool) -> Result<common::Seen> {
    let mut conn = reader.get_ref().try_clone()?;
    match protocol::read_message(&mut reader, 1 << 20)? {
        Some(protocol::Message::Line(line))
            if matches!(
                serde_json::from_str::<Outbound>(&line)?,
                Outbound::Hello { .. }
            ) => {}
        _ => return Err("first message is not hello".into()),
    }
    let assigned = spec
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    send(&mut conn, &Inbound::Spec { spec, plan: None })?;
    common::converse(
        assigned,
        cancel,
        || match protocol::read_message(&mut reader, 1 << 20)? {
            Some(protocol::Message::Event(bytes)) => {
                Ok(common::Message::Event(moruna_event::decode(&bytes)?))
            }
            Some(protocol::Message::Line(line)) => {
                Ok(common::Message::Control(serde_json::from_str(&line)?))
            }
            None => Err("engine closed before exit".into()),
        },
        |message| send(&mut conn, message),
    )
}
fn emit(args: &[String]) -> Result<()> {
    if args.len() != 5 && args.len() != 6 {
        return Err("usage: kd3-peer --emit-job DIGEST_FILE CASE RUN_ID OUT [PATHS.json]".into());
    }
    let digest = common::image_digest(Path::new(&args[1]))?;
    let case = args[2].parse::<usize>()?;
    if !(1..=5).contains(&case) {
        return Err("case must be1..5".into());
    }
    let mut job = cases(digest.trim())[case - 1].1.clone();
    job["run_id"] = json!(args[3]);
    if args.len() == 6 {
        let paths: Value = serde_json::from_slice(&fs::read(&args[5])?)?;
        for (pointer, value) in paths.as_object().ok_or("paths must be JSON object")? {
            if !PATHS.contains(&pointer.as_str()) {
                return Err("not an approved fixture path".into());
            }
            *job.pointer_mut(pointer)
                .ok_or("path does not exist in selected case")? = value.clone();
        }
    }
    checked_job(&cases(digest.trim())[case - 1].1, &job)?;
    let mut out = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args[4])?;
    serde_json::to_writer_pretty(&mut out, &job)?;
    out.write_all(b"\n")?;
    Ok(())
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--emit-job") {
        return emit(&args);
    }
    if args.len() < 4 {
        return Err(
            "usage: kd3-peer LOG UDS PORT DIGEST_FILE [--case 1..5] [--job actual.mjob]".into(),
        );
    }
    let mut selected = None;
    let mut actual_job = None;
    let (options, remainder) = args[4..].as_chunks::<2>();
    for option in options {
        match option[0].as_str() {
            "--case" if selected.is_none() => {
                let case = option[1].parse::<usize>()?;
                if !(1..=5).contains(&case) {
                    return Err("--case must be1..5".into());
                }
                selected = Some(case);
            }
            "--job" if actual_job.is_none() => {
                actual_job = Some(moruna_vmm::session::spec_value(&fs::read(&option[1])?)?);
            }
            _ => return Err("unknown/duplicate peer option".into()),
        }
    }
    if !remainder.is_empty() || actual_job.is_some() && selected.is_none() {
        return Err("--job requires one exact --case; every option requires a value".into());
    }
    let mut log = fs::File::create(&args[0])?;
    let digest = common::image_digest(Path::new(&args[3]))?;
    let digest = digest.as_str();
    let cases = cases(digest);
    let mut failures = 0;
    for (n, (name, spec)) in cases.into_iter().enumerate() {
        if selected.is_some_and(|selected| selected != n + 1) {
            continue;
        }
        if let Some(actual) = &actual_job {
            checked_job(&spec, actual)?;
        }
        let spec = actual_job.clone().unwrap_or(spec);
        let result = (|| -> Result<String> {
            let seen = converse(
                connect(Path::new(&args[1]), args[2].parse()?)?,
                spec,
                name == "cancel",
            )?;
            common::verdict(n, &seen, digest)
        })();
        match result {
            Ok(why) => writeln!(log, "embedder: ok: run {} ({name}): {why}", n + 1)?,
            Err(error) => {
                failures += 1;
                writeln!(log, "embedder: FAIL: run {} ({name}): {error}", n + 1)?;
            }
        }
        log.flush()?;
    }
    if failures > 0 {
        return Err(format!("{failures} actual KD2 runs failed").into());
    }
    Ok(())
}
