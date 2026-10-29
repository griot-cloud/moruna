//! LX-T10 to LX-T17 (SDD 28, k): unmodified Linux programs, from the image the boot's
//! second image filesystem serves, run through the Linux personality server:
//!
//! - a static musl program prints, reads its image, writes `/tmp` and exits 0 (LX-T10);
//! - eight musl threads sum in parallel with `clone` and `futex`, and are joined
//!   (LX-T11);
//! - a dynamically linked glibc program runs through `ld-linux` (LX-T12);
//! - CPython 3.14 from the official image's layers computes 20! as JSON (LX-T13);
//! - `socket` is refused with `ENOSYS`, the process goes on, and it is counted (LX-T14);
//! - a Linux process reaches no handle, no other task's files (LX-T15);
//! - the forward's round trip and the fast calls, measured (LX-T16).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use linux_protocol::{Bytes, Ended, how, linux};
use moruna_svc::{Channel, Console, deadline_in, now, println, start};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;

fn b(s: &str) -> Bytes {
    Bytes(s.as_bytes().to_vec())
}

fn env() -> Vec<Bytes> {
    vec![
        b("PATH=/usr/local/bin:/usr/bin:/bin"),
        b("HOME=/tmp"),
        b("LANG=C.UTF-8"),
    ]
}

struct Run {
    ended: Ended,
    text: String,
    seconds: u64,
}

fn run_program(c: &mut linux::Client, argv: &[&str]) -> Result<Run, String> {
    let start = now();
    let ended = c
        .run(b(argv[0]), argv.iter().map(|a| b(a)).collect(), env())
        .map_err(|e| format!("run {}: {e:?}", argv[0]))?;
    let text = String::from_utf8_lossy(&ended.output.0).into_owned();
    Ok(Run {
        ended,
        text,
        seconds: (now() - start) / SECONDS,
    })
}

fn exited_zero(r: &Run, what: &str) -> Result<(), String> {
    if r.ended.how != how::EXITED || r.ended.code != 0 {
        return Err(format!(
            "{what} ended (how {}, code {}) with output `{}`",
            r.ended.how,
            r.ended.code,
            r.text.trim_end()
        ));
    }
    Ok(())
}

fn refused(r: &Run) -> String {
    moruna_linux::refused::Refused::render(&r.ended.refused)
}

fn check(out: &Console, name: &str, result: Result<String, String>) -> bool {
    match result {
        Ok(detail) => {
            println!(out, "KC1 {}: ok: {}", name, detail);
            true
        }
        Err(why) => {
            println!(out, "KC1 {}: FAIL: {}", name, why);
            false
        }
    }
}

fn hello(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/hello", "one", "two"])?;
    exited_zero(&r, "hello")?;
    let want = "hello from musl (/kc1/bin/hello, 3 args): the image says 'Moruna KC1 greets you'; \
                /tmp gave back 'written to the scratch directory'";
    if !r.text.contains(want) {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(format!(
        "printed, read /kc1/greeting.txt, wrote and read back /tmp/out.txt, exited 0 ({} calls served, refused: {})",
        r.ended.served,
        refused(&r)
    ))
}

fn threads(c: &mut linux::Client, cpus: usize) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/threads"])?;
    exited_zero(&r, "threads")?;
    let want = "threads: 8 threads summed 1..4000000 = 8000002000000 (want 8000002000000)";
    if !r.text.contains(want) {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(format!(
        "8 threads summed 1..4000000 to 8000002000000 on {} CPUs and were joined (refused: {})",
        cpus,
        refused(&r)
    ))
}

fn dynamic(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/dynamic"])?;
    exited_zero(&r, "dynamic")?;
    if !r.text.contains("dynamic glibc ") || !r.text.contains("hello through ld-linux") {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(format!("{} (refused: {})", r.text.trim_end(), refused(&r)))
}

fn cpython(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(
        c,
        &[
            "/usr/local/bin/python3",
            "-c",
            "import json, math; print(json.dumps(math.factorial(20)))",
        ],
    )?;
    exited_zero(&r, "python3")?;
    if r.text.trim_end() != "2432902008176640000" {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(format!(
        "python3 -c printed 2432902008176640000 in {} s; {} calls served; refused: {}",
        r.seconds,
        r.ended.served,
        refused(&r)
    ))
}

fn refuse(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/refuse"])?;
    exited_zero(&r, "refuse")?;
    if !r.text.contains("socket() was refused with ENOSYS") {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    if !r.ended.refused.contains(&(41, 1)) {
        return Err(format!("socket is not counted: refused {}", refused(&r)));
    }
    Ok(format!(
        "socket() answered ENOSYS, the process went on and exited 0, and the count says {}",
        refused(&r)
    ))
}

fn isolate(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/isolate"])?;
    exited_zero(&r, "isolate")?;
    if !r.text.contains("were all BadHandle") {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(String::from(r.text.trim_end()))
}

fn bench(c: &mut linux::Client) -> Result<String, String> {
    let r = run_program(c, &["/kc1/bin/bench"])?;
    exited_zero(&r, "bench")?;
    if !r.text.contains("bench: forwarded call") {
        return Err(format!("printed `{}`", r.text.trim_end()));
    }
    Ok(String::from(r.text.trim_end()))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listener) = start::take("server").map(Channel) else {
        println!(out, "KC1 linux-test: FAIL: no `server` slot");
        return 1;
    };
    let cpus = start::cpus();
    let channel = match moruna_svc::connect(&listener) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KC1 linux-test: FAIL: connecting: {e:?}");
            return 1;
        }
    };
    let mut c = match linux::Client::connect(channel, deadline_in(600 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KC1 linux-test: FAIL: hello: {e:?}");
            return 1;
        }
    };
    c.core().set_timeout(1800 * SECONDS);
    let mut ok = true;
    ok &= check(&out, "musl hello", hello(&mut c));
    ok &= check(&out, "musl threads", threads(&mut c, cpus));
    ok &= check(&out, "glibc dynamic", dynamic(&mut c));
    ok &= check(&out, "refused call", refuse(&mut c));
    ok &= check(&out, "isolation", isolate(&mut c));
    ok &= check(&out, "forward", bench(&mut c));
    ok &= check(&out, "cpython", cpython(&mut c));
    if ok { 0 } else { 1 }
}
