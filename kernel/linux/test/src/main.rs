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
//! - the forward's round trip and the fast calls, measured (LX-T16);
//! - a program run with `run_granted` (1.2) holds the handle it was granted, named on its
//!   start page, is told its budget, and has eventfd, epoll and rename (KD2-T4, SDD 30);
//!   the budget is an account carved here (1.3), which the program reads itself;
//! - a program that writes past its account is ended by the kernel with the account
//!   named, its peak at the account's limit, and the personality runs the next program
//!   as before (KB5-T1, SDD 20 f.18).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use linux_protocol::{Bytes, Ended, Grant, how, linux};
use moruna_svc::{Account, Channel, Console, Rights, deadline_in, now, println, start};

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
    if !r.text.contains("dynamic glibc ")
        || !r.text.contains("hello through ld-linux")
        || !r
            .text
            .contains("dynamic: readonly subrange alias, private copy and descriptor reuse ok")
    {
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

/// An account of `limit` bytes carved out of this process's own, named `name`.
fn carve(limit: u64, name: &str) -> Result<Account, String> {
    Account::carve(limit, name).map_err(|e| format!("an account of {limit} bytes: {}", e.name()))
}

/// KD2-T4: `run_granted` with one channel endpoint named `probe` and a 256 MiB account.
fn granted(c: &mut linux::Client) -> Result<String, String> {
    let (mine, theirs) = Channel::create().map_err(|e| format!("channel_create: {}", e.name()))?;
    let grant = Grant {
        handles: vec![(b("probe"), theirs.0)],
        account: carve(256 << 20, "linux-test.granted")?.0,
        scratch: None,
    };
    let path = "/kc1/bin/granted";
    let ended = c
        .run_granted(b(path), vec![b(path)], env(), grant)
        .map_err(|e| format!("run_granted {path}: {e:?}"))?;
    drop(mine);
    let r = Run {
        text: String::from_utf8_lossy(&ended.output.0).into_owned(),
        ended,
        seconds: 0,
    };
    exited_zero(&r, "granted")?;
    let Some(line) = r.text.lines().find(|l| l.starts_with("granted: ok:")) else {
        return Err(format!("printed `{}`", r.text.trim_end()));
    };
    Ok(format!(
        "{} ({} calls served, refused: {})",
        line.trim_start_matches("granted: ok: "),
        r.ended.served,
        refused(&r)
    ))
}

/// KB5-T1: a program granted an account of 16 MiB writes past it. The kernel ends it at
/// the first page the account has no room for, with the account named on the console;
/// its account's peak is the limit, less less than a page; and the personality runs the
/// next program as before.
fn hog(c: &mut linux::Client) -> Result<String, String> {
    const LIMIT: u64 = 16 << 20;
    let account = carve(LIMIT, "linux-test.hog")?;
    let kept = account
        .0
        .duplicate(Rights::READ)
        .map(Account)
        .map_err(|e| format!("duplicating the account: {}", e.name()))?;
    let grant = Grant {
        handles: Vec::new(),
        account: account.0,
        scratch: None,
    };
    let path = "/kc1/bin/hog";
    let ended = c
        .run_granted(b(path), vec![b(path)], env(), grant)
        .map_err(|e| format!("run_granted {path}: {e:?}"))?;
    let text = String::from_utf8_lossy(&ended.output.0).into_owned();
    if ended.how != how::FAULTED || text.contains("hog: FAIL") {
        return Err(format!(
            "hog ended {} with code {}, printing `{}`",
            ended.how,
            ended.code,
            text.trim_end()
        ));
    }
    if !text.contains("hog: its machine has 16 MiB") {
        return Err(format!("hog printed `{}`", text.trim_end()));
    }
    let wrote = text
        .lines()
        .filter_map(|l| l.strip_prefix("hog: wrote MiB "))
        .filter_map(|n| n.trim().parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let usage = kept
        .usage()
        .map_err(|e| format!("reading its account: {}", e.name()))?;
    // It was ended for want of room for one more zero page.
    let page = moruna_svc::charge::ZERO_PAGE;
    if usage.peak > LIMIT || usage.peak + page <= LIMIT || wrote == 0 || wrote >= LIMIT >> 20 {
        return Err(format!(
            "hog wrote {wrote} MiB and its account's peak is {} of {LIMIT}",
            usage.peak
        ));
    }
    // The personality runs on.
    let after = run_program(c, &["/kc1/bin/hello"])?;
    exited_zero(&after, "hello after the hog")?;
    Ok(format!(
        "hog wrote {wrote} MiB of its 16 MiB account and was ended by the kernel at the next page; its account's peak {} bytes, {} under its limit; hello ran after it",
        usage.peak,
        LIMIT - usage.peak
    ))
}

/// KB5-T9: the account protocol cannot decode old unchecked-budget requests.
fn old_major_refused(listener: &Channel) -> Result<(), String> {
    let channel = moruna_svc::connect(listener).map_err(|e| format!("connecting: {e:?}"))?;
    let old = moruna_svc::protocol::ProtocolId::new(linux::ID.name, 1, 2);
    match moruna_svc::ClientCore::connect(channel, old, deadline_in(5 * SECONDS)) {
        Err(moruna_svc::CallError::Refused(refusal))
            if refusal.reason == moruna_svc::protocol::Refusal::MajorMismatch
                && refusal.server == linux::ID.hello() =>
        {
            Ok(())
        }
        other => Err(format!(
            "the old major was not refused before requests: {:?}",
            other.map(|_| ())
        )),
    }
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listener) = start::take("server").map(Channel) else {
        println!(out, "KC1 linux-test: FAIL: no `server` slot");
        return 1;
    };
    if let Err(why) = old_major_refused(&listener) {
        println!(out, "KB5 linux protocol: FAIL: {}", why);
        return 1;
    }
    println!(
        out,
        "KB5 linux protocol: ok: major 1 rejected at hello; account protocol 2.0 required"
    );
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
    match granted(&mut c) {
        Ok(detail) => println!(out, "KD2 granted: ok: {}", detail),
        Err(why) => {
            println!(out, "KD2 granted: FAIL: {}", why);
            ok = false;
        }
    }
    match hog(&mut c) {
        Ok(detail) => println!(out, "KB5 hog: ok: {}", detail),
        Err(why) => {
            println!(out, "KB5 hog: FAIL: {}", why);
            ok = false;
        }
    }
    if ok { 0 } else { 1 }
}
