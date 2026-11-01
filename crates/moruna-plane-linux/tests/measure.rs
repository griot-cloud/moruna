//! KA2's measurements on Linux, with a producer and a consumer that are processes of their own
//! (this binary run again as `plane_role`), as DP-T10, DP-T14 and DP-T15 are on the kernel:
//!
//! - zero copy: a record of 64 MiB of values written by the producer in its window and put, read
//!   by the consumer in place. The proof is read from the mappings: the producer's window, the
//!   engine's batch and the consumer's batch are one memfd (the same inode in each process's
//!   `/proc/self/maps`), the consumer's mapping holds no page of its own (`Anonymous` and
//!   `Private_Dirty` 0 in its `smaps`, with every page resident), and, where the kernel shows
//!   frame numbers (`CAP_SYS_ADMIN`), pages 0, middle and last are the same frames in all three;
//! - throughput: 1,000 batches of 1 MiB from producer to consumer, in order, four windows in
//!   flight, with the producer's and the consumer's calls timed;
//! - port calls: 2,000 round trips, least and median.
//!
//! Each prints a `ka2 ...` line with its numbers.
#![cfg(target_os = "linux")]

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use moruna_plane::{DataPlane, Event, Shape};
use moruna_plane_linux::task::Plane;
use moruna_plane_linux::{LinuxPlane, sys};
use support::process::{self, Task};

const PRODUCE: u8 = 1;
const CONSUME: u8 = 2;
const CALL: u8 = 3;

fn role(kind: u8, rows: u64, count: u64) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out
}

fn words(bytes: &[u8]) -> (u8, u64, u64) {
    let w = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    (bytes[0], w(1), w(9))
}

/// The frame of the page holding `addr` in this process, when the kernel shows it.
fn frame(addr: u64) -> Option<u64> {
    let mut f = std::fs::File::open("/proc/self/pagemap").ok()?;
    f.seek(SeekFrom::Start(addr / 4096 * 8)).ok()?;
    let mut e = [0u8; 8];
    f.read_exact(&mut e).ok()?;
    let e = u64::from_le_bytes(e);
    let pfn = e & ((1 << 55) - 1);
    (e >> 63 == 1 && pfn != 0).then_some(pfn)
}

/// The inode of the mapping that holds `addr`, and its `smaps` lines.
fn mapping(addr: u64) -> (u64, BTreeMap<String, u64>) {
    let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
    let mut inode = 0;
    let mut fields = BTreeMap::new();
    let mut inside = false;
    for line in smaps.lines() {
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or("");
        if let Some((from, to)) = first.split_once('-')
            && let (Ok(from), Ok(to)) = (u64::from_str_radix(from, 16), u64::from_str_radix(to, 16))
        {
            inside = (from..to).contains(&addr);
            if inside {
                inode = line
                    .split_whitespace()
                    .nth(4)
                    .and_then(|i| i.parse().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        if inside && first.ends_with(':') {
            if let Some(kb) = parts.next().and_then(|v| v.parse::<u64>().ok()) {
                fields.insert(first.trim_end_matches(':').to_string(), kb);
            }
        }
    }
    (inode, fields)
}

fn median(mut v: Vec<u64>) -> (u64, u64) {
    v.sort_unstable();
    (v[0], v[v.len() / 2])
}

fn line(results: &mut std::fs::File, text: String) {
    process::report(results, format!("{text}\n").as_bytes());
}

/// A producer, a consumer or a caller, started by a measurement.
#[test]
#[ignore = "a task of the measurements, which start it as a process of its own"]
fn plane_role() {
    let Some((socket, input, mut out)) = process::task() else {
        return;
    };
    let (kind, rows, count) = words(&input);
    let plane = Plane::new(socket).expect("the plane");
    match kind {
        PRODUCE => {
            let framing = moruna_testkit::plane::framing(rows as usize).expect("a framing");
            let len = 4096 + rows * 8;
            // The values, made once: each batch is a copy of them, its first value its number.
            let values: Vec<u8> = (0..rows).flat_map(|v| v.to_le_bytes()).collect();
            let mut output = plane.output(0).expect("output 0");
            let (mut gets, mut puts) = (Vec::new(), Vec::new());
            for i in 0..count {
                let started = Instant::now();
                let mut w = output.window().expect("window").expect("a window");
                gets.push(started.elapsed().as_micros() as u64);
                let bytes = w.bytes();
                bytes[..4096].copy_from_slice(&framing);
                bytes[4096..len as usize].copy_from_slice(&values);
                bytes[4096..4104].copy_from_slice(&i.to_le_bytes());
                if count == 1 {
                    let pages = len.div_ceil(4096);
                    let at = |p: u64| w.addr() + p * 4096;
                    let frames: Vec<String> = [0, pages / 2, pages - 1]
                        .iter()
                        .map(|p| frame(at(*p)).map_or("hidden".into(), |f| f.to_string()))
                        .collect();
                    line(&mut out, format!("frames {}", frames.join(" ")));
                    line(&mut out, format!("inode {}", mapping(w.addr()).0));
                }
                let started = Instant::now();
                w.put(0, len).map_err(|(e, _)| e).expect("put");
                puts.push(started.elapsed().as_micros() as u64);
            }
            let (g, gm) = median(gets);
            let (p, pm) = median(puts);
            line(&mut out, format!("window {g} {gm}"));
            line(&mut out, format!("put {p} {pm}"));
        }
        CONSUME => {
            let mut input = plane.input(0).expect("input 0");
            let mut gets = Vec::new();
            let mut n = 0u64;
            loop {
                let started = Instant::now();
                let Some(b) = input.get().expect("get") else {
                    break;
                };
                gets.push(started.elapsed().as_micros() as u64);
                let bytes = b.bytes();
                let first = u64::from_le_bytes(bytes[4096..4104].try_into().expect("8"));
                assert_eq!(first, n, "batches in order");
                let last = bytes.len() - 8;
                let tail = u64::from_le_bytes(bytes[last..].try_into().expect("8"));
                assert_eq!(tail, rows - 1, "the last value");
                if count == 1 {
                    // Every page read, so every page is mapped.
                    let mut s = 0u64;
                    for p in bytes.chunks(4096) {
                        s = s.wrapping_add(u64::from(p[0]));
                    }
                    std::hint::black_box(s);
                    let pages = (bytes.len() as u64).div_ceil(4096);
                    let at = |p: u64| b.addr() + p * 4096;
                    let frames: Vec<String> = [0, pages / 2, pages - 1]
                        .iter()
                        .map(|p| frame(at(*p)).map_or("hidden".into(), |f| f.to_string()))
                        .collect();
                    line(&mut out, format!("frames {}", frames.join(" ")));
                    let (inode, fields) = mapping(b.addr());
                    line(&mut out, format!("inode {inode}"));
                    for k in [
                        "Rss",
                        "Anonymous",
                        "Private_Dirty",
                        "Private_Clean",
                        "Shared_Clean",
                    ] {
                        line(
                            &mut out,
                            format!("smaps.{k} {}", fields.get(k).copied().unwrap_or(u64::MAX)),
                        );
                    }
                }
                n += 1;
            }
            let (g, gm) = median(gets);
            line(&mut out, format!("got {n}"));
            line(&mut out, format!("get {g} {gm}"));
        }
        CALL => {
            let mut port = plane.port().expect("port");
            let mut times = Vec::new();
            for i in 0..count {
                let started = Instant::now();
                let reply = port
                    .call(1, &i.to_le_bytes())
                    .expect("call")
                    .expect("a reply");
                times.push(started.elapsed().as_micros() as u64);
                assert_eq!(reply.bytes(), &i.to_le_bytes());
            }
            let (least, med) = median(times);
            line(&mut out, format!("call {least} {med}"));
        }
        other => panic!("role {other}"),
    }
    // SAFETY: the task's end, without unwinding through the test harness.
    unsafe { libc::_exit(0) };
}

fn report(task: &mut Task) -> BTreeMap<String, Vec<String>> {
    let (status, bytes) = task.finish();
    assert!(status.success(), "a role failed: {status}");
    String::from_utf8(bytes)
        .expect("text")
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace().map(str::to_string);
            Some((w.next()?, w.collect()))
        })
        .collect()
}

fn shape(inputs: u32, outputs: u32, port: bool) -> Shape {
    Shape {
        inputs,
        outputs,
        port,
    }
}

fn start(p: &mut LinuxPlane, shape: Shape, input: Vec<u8>) -> (moruna_plane::TaskId, Task) {
    let (task, end) = p.open(shape).expect("open");
    let t = process::start("plane_role", end, &input);
    p.attach(task, sys::pidfd(t.child.id()).expect("pidfd"))
        .expect("attach");
    (task, t)
}

fn next(p: &mut LinuxPlane) -> Event {
    p.next(sys::now() + 30_000_000_000)
        .expect("next")
        .expect("an event within 30 s")
}

#[test]
fn ka2_zero_copy_a_64_mib_batch_is_the_same_pages_in_producer_engine_and_consumer() {
    let rows: u64 = 8 << 20;
    let len = 4096 + rows * 8;
    let mut p = LinuxPlane::new(1 << 28);
    let (producer, mut ptask) = start(&mut p, shape(0, 1, false), role(PRODUCE, rows, 1));
    p.grant(producer, 0, len).expect("grant");
    let batch = loop {
        match next(&mut p) {
            Event::Put {
                batch,
                rows: r,
                bytes,
                ..
            } => {
                assert_eq!((r, bytes), (rows, len));
                break batch;
            }
            other => panic!("{other:?}"),
        }
    };
    let (consumer, mut ctask) = start(&mut p, shape(1, 0, false), role(CONSUME, rows, 1));
    p.offer(consumer, 0, batch).expect("offer");
    p.finish(consumer, 0).expect("finish");
    let engine = p.map(batch).expect("the engine's mapping");
    let mut s = 0u64;
    for page in engine.bytes().chunks(4096) {
        s = s.wrapping_add(u64::from(page[0]));
    }
    std::hint::black_box(s);
    let pages = len.div_ceil(4096);
    let engine_frames: Vec<String> = [0, pages / 2, pages - 1]
        .iter()
        .map(|pg| frame(engine.addr() + pg * 4096).map_or("hidden".into(), |f| f.to_string()))
        .collect();
    let (fd, _, _) = p.batch(batch).expect("batch");
    let (_, engine_inode, _) = sys::stat(fd).expect("stat");
    let (engine_map_inode, _) = mapping(engine.addr());
    let mut ended = 0;
    while ended < 2 {
        if let Event::Ended { .. } = next(&mut p) {
            ended += 1;
        }
    }
    let made = report(&mut ptask);
    let read = report(&mut ctask);
    let inode = |r: &BTreeMap<String, Vec<String>>| r["inode"][0].parse::<u64>().expect("inode");
    assert_eq!(
        inode(&made),
        engine_inode,
        "the producer's window is the batch's memfd"
    );
    assert_eq!(
        inode(&read),
        engine_inode,
        "the consumer's batch is the same memfd"
    );
    assert_eq!(engine_map_inode, engine_inode);
    let smaps = |k: &str| read[&format!("smaps.{k}")][0].parse::<u64>().expect("kB");
    let rss = smaps("Rss");
    assert_eq!(smaps("Anonymous"), 0, "no page of the consumer's own");
    assert_eq!(smaps("Private_Dirty"), 0, "no page copied on write");
    assert_eq!(
        rss,
        pages * 4,
        "every page of the batch resident in the consumer's mapping"
    );
    let frames = [&made["frames"], &engine_frames, &read["frames"]];
    let shown = frames.iter().all(|f| f.iter().all(|x| x != "hidden"));
    if shown {
        assert_eq!(frames[0], frames[1], "producer and engine: the same frames");
        assert_eq!(frames[1], frames[2], "engine and consumer: the same frames");
    }
    println!(
        "ka2 zero copy: ok: {len} bytes ({pages} pages): inode {engine_inode} in producer, engine \
         and consumer; consumer Rss {rss} kB, Anonymous 0 kB, Private_Dirty 0 kB; frames of pages \
         0, {}, {}: producer {}, engine {}, consumer {}",
        pages / 2,
        pages - 1,
        made["frames"].join(" "),
        engine_frames.join(" "),
        read["frames"].join(" ")
    );
}

#[test]
fn ka2_throughput_1000_batches_of_1_mib_in_order() {
    let rows: u64 = 128 << 10;
    let len = 4096 + rows * 8;
    const COUNT: u64 = 1000;
    const WINDOWS: u64 = 4;
    let mut p = LinuxPlane::new(1 << 28);
    let (producer, mut ptask) = start(&mut p, shape(0, 1, false), role(PRODUCE, rows, COUNT));
    let (consumer, mut ctask) = start(&mut p, shape(1, 0, false), role(CONSUME, rows, COUNT));
    let started = Instant::now();
    for _ in 0..WINDOWS {
        p.grant(producer, 0, len).expect("grant");
    }
    let (mut granted, mut put, mut freed, mut peak) = (WINDOWS, 0, 0, 0);
    let mut ended = 0;
    while ended < 2 {
        match next(&mut p) {
            Event::Put { batch, rows: r, .. } => {
                assert_eq!(r, rows);
                p.offer(consumer, 0, batch).expect("offer");
                p.drop_batch(batch).expect("drop");
                put += 1;
                if put == COUNT {
                    p.finish(consumer, 0).expect("finish");
                }
                let r = p.residency();
                peak = peak.max(r.made);
            }
            Event::Freed { .. } => {
                freed += 1;
                if granted < COUNT {
                    p.grant(producer, 0, len).expect("grant");
                    granted += 1;
                }
            }
            Event::Released { .. } => {}
            Event::Ended { .. } => ended += 1,
            other => panic!("{other:?}"),
        }
    }
    let took = started.elapsed();
    let made = report(&mut ptask);
    let read = report(&mut ctask);
    assert_eq!(read["got"][0], COUNT.to_string());
    assert_eq!(freed, COUNT);
    let r = p.residency();
    assert_eq!((r.batches, r.windows, r.made), (0, 0, 0));
    println!(
        "ka2 throughput: ok: {COUNT} batches of {len} bytes in {:.3} s: {:.0} batches/s, {:.2} \
         GiB/s; {WINDOWS} windows in flight, at most {} MiB of memfds at once; producer window \
         least {} us (median {}), put least {} us (median {}); consumer get least {} us (median \
         {})",
        took.as_secs_f64(),
        COUNT as f64 / took.as_secs_f64(),
        (COUNT * len) as f64 / took.as_secs_f64() / (1u64 << 30) as f64,
        peak >> 20,
        made["window"][0],
        made["window"][1],
        made["put"][0],
        made["put"][1],
        read["get"][0],
        read["get"][1],
    );
}

#[test]
fn ka2_port_calls_2000_round_trips() {
    const COUNT: u64 = 2000;
    let mut p = LinuxPlane::new(1 << 20);
    let (_, mut task) = start(&mut p, shape(0, 0, true), role(CALL, 0, COUNT));
    let mut answered = 0;
    loop {
        match next(&mut p) {
            Event::Call {
                call,
                port: 1,
                request,
            } => {
                p.answer(call, Ok(&request)).expect("answer");
                answered += 1;
            }
            Event::Ended { .. } => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(answered, COUNT);
    let r = report(&mut task);
    println!(
        "ka2 port: ok: {COUNT} round trips, least {} us (median {} us)",
        r["call"][0], r["call"][1]
    );
}
