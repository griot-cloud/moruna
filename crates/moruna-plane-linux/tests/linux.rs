//! KA2 on Linux, beyond the suite: what only a task that does not use the library can try. The
//! test plays the task on the raw socket: garbage, calls with wrong arguments, a window kept
//! mapped writable at its put, reshaped or sealed against the engine, and every way left to
//! write a record or a batch once it is the engine's. And the library from many threads.
#![cfg(target_os = "linux")]

mod support;

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use moruna_abi::Error;
use moruna_plane::{DataPlane, Event, Shape, TaskId};
use moruna_plane_linux::LinuxPlane;
use moruna_plane_linux::sys::{self, Access, Map};
use moruna_plane_linux::task::Plane;
use moruna_plane_linux::wire::{self, Header};

fn shape(inputs: u32, outputs: u32, port: bool) -> Shape {
    Shape {
        inputs,
        outputs,
        port,
    }
}

/// Every event the plane has within a short wait.
fn pump(p: &mut LinuxPlane) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(e) = p.next(sys::now() + 50_000_000).expect("next") {
        out.push(e);
    }
    out
}

fn ask(end: &OwnedFd, h: Header, body: &[u8], fd: Option<&OwnedFd>) {
    let mut m = h.bytes().to_vec();
    m.extend_from_slice(body);
    sys::send(end.as_fd(), &m, fd.map(|f| f.as_fd()), true).expect("send");
}

/// The engine's answer, once `p` has read what was asked.
fn answer(p: &mut LinuxPlane, end: &OwnedFd) -> (Header, Option<OwnedFd>) {
    let _ = p.next(sys::now() + 50_000_000).expect("next");
    receive_answer(end)
}

fn receive_answer(end: &OwnedFd) -> (Header, Option<OwnedFd>) {
    let mut buf = [0u8; 64];
    let got = sys::recv(end.as_fd(), &mut buf, true).expect("an answer");
    let h = Header::read(&buf[..got.len]).expect("a header");
    (h, got.fds.into_iter().next())
}

fn nothing(end: &OwnedFd) -> bool {
    let mut buf = [0u8; 64];
    matches!(sys::recv(end.as_fd(), &mut buf, false), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
}

fn put(slot: u32, offset: u64, len: u64) -> Header {
    let mut h = Header::new(wire::PUT, slot);
    h.a = offset;
    h.b = len;
    h
}

fn status(h: &Header) -> Option<Error> {
    h.status()
}

#[test]
fn ka2_a_task_off_the_library_gets_the_kernels_errors_and_the_engine_carries_on() {
    let mut p = LinuxPlane::new(1 << 20);
    let (task, end) = p.open(shape(1, 1, true)).expect("open");
    // Bytes that are no header, and an empty message: nothing to answer.
    sys::send(end.as_fd(), &[1, 2, 3], None, true).expect("send");
    sys::send(end.as_fd(), &[], None, true).expect("send");
    assert!(pump(&mut p).is_empty());
    assert!(nothing(&end), "no answer to what is no request");
    ask(&end, Header::new(99, 0), &[], None);
    assert_eq!(status(&answer(&mut p, &end).0), Some(Error::BadSyscall));
    ask(&end, Header::new(wire::GET, 7), &[], None);
    assert_eq!(status(&answer(&mut p, &end).0), Some(Error::BadHandle));
    for (h, want) in [
        (put(0, 0, 64), Error::InvalidArgs),
        (put(1, 0, 0), Error::InvalidArgs),
        (put(1, 100, 64), Error::InvalidArgs),
        (put(1, 0, 64), Error::BadState),
        (put(9, 0, 64), Error::BadHandle),
    ] {
        ask(&end, h, &[], None);
        let (got, _) = answer(&mut p, &end);
        assert_eq!(
            (got.kind, got.a, status(&got)),
            (wire::ERROR, u64::from(wire::PUT), Some(want))
        );
    }
    // Port calls: a length that is not what came, one past the bound, a message past the
    // largest one.
    let mut call = Header::new(wire::CALL, 0);
    call.a = 5;
    call.c = 10;
    ask(&end, call, b"abc", None);
    let (got, _) = answer(&mut p, &end);
    assert_eq!(
        (got.kind, got.a, status(&got)),
        (wire::REPLY, 5, Some(Error::InvalidArgs))
    );
    call.c = moruna_abi::plane::request::MAX as u64 + 1;
    ask(&end, call, &[], None);
    assert_eq!(status(&answer(&mut p, &end).0), Some(Error::Limit));
    call.c = 70_000;
    ask(&end, call, &vec![0u8; 70_000], None);
    assert_eq!(status(&answer(&mut p, &end).0), Some(Error::Limit));
    // A descriptor sent to the engine is closed there; the request beside it is served.
    let stray = sys::memfd(4096).expect("memfd");
    p.grant(task, 0, 8192).expect("grant");
    ask(&end, Header::new(wire::GET, 1), &[], Some(&stray));
    let (got, window) = answer(&mut p, &end);
    assert_eq!((got.kind, got.b), (wire::WINDOW, 8192));
    assert!(window.is_some());
    ask(&end, put(1, 4096, 8192), &[], None);
    assert_eq!(status(&answer(&mut p, &end).0), Some(Error::OutOfRange));
    assert!(pump(&mut p).is_empty(), "no event for any of it");
    drop((end, window));
    assert_eq!(pump(&mut p), vec![Event::Ended { task }]);
    let r = p.residency();
    assert_eq!((r.windows, r.made - r.pooled, r.batches), (0, 0, 0));
}

/// A window granted and taken by the raw task: its memfd, mapped writable.
fn window(p: &mut LinuxPlane, end: &OwnedFd, task: TaskId, slot: u32, len: u64) -> (OwnedFd, Map) {
    p.grant(task, slot, len).expect("grant");
    ask(end, Header::new(wire::GET, slot), &[], None);
    let (got, fd) = answer(p, end);
    assert_eq!(got.kind, wire::WINDOW);
    let fd = fd.expect("the window's memfd");
    let map = Map::new(fd.as_fd(), 0, got.b as usize, Access::Write).expect("mapped");
    (fd, map)
}

fn refused(events: &[Event], task: TaskId) -> String {
    match events {
        [
            Event::Refused {
                task: t,
                output: 0,
                reason,
            },
            Event::Ended { task: e },
        ] if *t == task && *e == task => reason.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn ka2_a_window_kept_mapped_writable_at_its_put_is_refused() {
    let record = support::fixtures().good;
    let mut p = LinuxPlane::new(1 << 22);
    let (task, end) = p.open(shape(0, 1, false)).expect("open");
    let (_fd, mut map) = window(&mut p, &end, task, 0, record.len() as u64);
    map.bytes_mut()[..record.len()].copy_from_slice(&record);
    ask(&end, put(0, 0, record.len() as u64), &[], None);
    let reason = refused(&pump(&mut p), task);
    assert!(reason.contains("mapped writable"), "{reason}");
    let r = p.residency();
    assert_eq!((r.batches, r.windows, r.made - r.pooled), (0, 0, 0));
}

#[test]
fn ka2_a_window_reshaped_or_sealed_against_the_engine_is_refused() {
    let record = support::fixtures().good;
    let len = record.len() as u64;
    for (how, says) in [
        ("shrink", "changed its window"),
        ("grow", "changed its window"),
        ("seal", "sealed its window"),
    ] {
        let mut p = LinuxPlane::new(1 << 22);
        let (task, end) = p.open(shape(0, 1, false)).expect("open");
        let (fd, mut map) = window(&mut p, &end, task, 0, len);
        map.bytes_mut()[..record.len()].copy_from_slice(&record);
        drop(map);
        // SAFETY: ftruncate and fcntl on a descriptor this test holds.
        let r = unsafe {
            match how {
                "shrink" => libc::ftruncate(fd.as_raw_fd(), 4096),
                "grow" => libc::ftruncate(fd.as_raw_fd(), 1 << 30),
                _ => libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SEAL),
            }
        };
        assert_eq!(r, 0, "{how}: {}", io::Error::last_os_error());
        ask(&end, put(0, 0, len), &[], None);
        let reason = refused(&pump(&mut p), task);
        assert!(reason.contains(says), "{how}: {reason}");
        assert_eq!(p.residency().batches, 0, "{how}");
    }
}

/// Every way a producer or a consumer could still write a sealed batch fails, and the bytes
/// are what was put.
#[test]
fn ka2_once_put_a_record_is_written_by_no_descriptor_and_no_mapping() {
    let record = support::fixtures().good;
    let len = record.len() as u64;
    let mut p = LinuxPlane::new(1 << 22);
    let (producer, end) = p.open(shape(0, 1, false)).expect("open");
    let (fd, mut map) = window(&mut p, &end, producer, 0, len);
    map.bytes_mut()[..record.len()].copy_from_slice(&record);
    drop(map);
    ask(&end, put(0, 0, len), &[], None);
    // next() processes PUT and returns its real Event::Put. Do not discard it
    // while receiving PUT_DONE and then expect it to appear a second time.
    let put_event = p.next(sys::now() + 50_000_000).expect("next");
    let (done, _) = receive_answer(&end);
    assert_eq!(done.kind, wire::PUT_DONE);
    let batch = match put_event.as_slice() {
        [
            Event::Put {
                batch,
                rows: 8,
                bytes,
                ..
            },
        ] if *bytes == len => *batch,
        other => panic!("{other:?}"),
    };
    assert_cannot_write(&fd, "the producer's descriptor of its window");
    // A consumer's descriptor of the batch.
    let (consumer, cend) = p.open(shape(1, 0, false)).expect("open");
    p.offer(consumer, 0, batch).expect("offer");
    ask(&cend, Header::new(wire::GET, 0), &[], None);
    let (got, bfd) = answer(&mut p, &cend);
    assert_eq!((got.kind, got.b), (wire::BATCH, len));
    let bfd = bfd.expect("the batch's memfd");
    assert_cannot_write(&bfd, "a consumer's descriptor of the batch");
    let theirs = Map::new(bfd.as_fd(), got.a, len as usize, Access::Read).expect("mapped");
    assert_eq!(theirs.bytes(), &record[..]);
    // A private mapping made writable takes the write itself; the batch does not change.
    // SAFETY: mprotect of the mapping above, then a store into it, which is now writable.
    unsafe {
        let at = theirs.addr() as *mut libc::c_void;
        assert_eq!(
            libc::mprotect(at, theirs.len(), libc::PROT_READ | libc::PROT_WRITE),
            0
        );
        std::ptr::write_volatile(at.cast::<u8>(), 0x77);
    }
    assert_eq!(theirs.bytes()[0], 0x77, "the private copy took it");
    let engine = p.map(batch).expect("the engine's mapping");
    assert_eq!(engine.bytes(), &record[..], "the batch is the bytes put");
    let fresh = Map::new(bfd.as_fd(), got.a, len as usize, Access::Read).expect("mapped");
    assert_eq!(fresh.bytes(), &record[..]);
}

fn assert_cannot_write(fd: &OwnedFd, what: &str) {
    let seals = sys::seals(fd.as_fd()).expect("seals");
    assert_eq!(seals & sys::SEALS, sys::SEALS, "{what}: sealed");
    assert!(
        sys::pwrite_all(fd.as_fd(), 0, b"x").is_err(),
        "{what}: pwrite"
    );
    let shared = Map::new(fd.as_fd(), 0, 4096, Access::Write);
    assert!(shared.is_err(), "{what}: a shared writable mapping");
    // SAFETY: ftruncate and fallocate on a descriptor this test holds.
    unsafe {
        assert_ne!(libc::ftruncate(fd.as_raw_fd(), 0), 0, "{what}: shrink");
        assert_ne!(libc::ftruncate(fd.as_raw_fd(), 1 << 30), 0, "{what}: grow");
        assert_ne!(
            libc::fallocate(
                fd.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                0,
                4096
            ),
            0,
            "{what}: punch"
        );
    }
}

/// The library's values used from many threads at once: two inputs read, a port called, and an
/// output written, all on one socket.
#[test]
fn ka2_the_library_serves_many_threads_on_one_socket() {
    let record = support::fixtures().good;
    let mut p = LinuxPlane::new(1 << 24);
    let (batch, rows) = p.load(&record).expect("load");
    assert_eq!(rows, 8);
    let (task, end) = p.open(shape(2, 1, true)).expect("open");
    let plane = Plane::new(end).expect("the plane");
    const EACH: usize = 40;
    for input in 0..2 {
        for _ in 0..EACH {
            p.offer(task, input, batch).expect("offer");
        }
        p.finish(task, input).expect("finish");
    }
    p.grant(task, 0, record.len() as u64).expect("grant");
    p.drop_batch(batch).expect("drop");
    std::thread::scope(|s| {
        // Every task borrows the same immutable fixture for precisely this scope. The
        // writer moves only this Copy slice, never the Vec borrowed by the readers.
        let record = record.as_slice();
        let readers: Vec<_> = (0..2)
            .map(|n| {
                let mut input = plane.input(n).expect("input");
                s.spawn(move || {
                    let mut got = 0;
                    while let Some(b) = input.get().expect("get") {
                        assert_eq!(b.bytes(), record);
                        got += 1;
                    }
                    got
                })
            })
            .collect();
        let mut port = plane.port().expect("port");
        let caller = s.spawn(move || {
            for i in 0..EACH as u64 {
                let reply = port
                    .call(i, &i.to_le_bytes())
                    .expect("call")
                    .expect("bytes");
                assert_eq!(reply.bytes(), &(i * 2).to_le_bytes());
            }
        });
        let mut output = plane.output(2).expect("output");
        let writer = s.spawn(move || {
            let mut w = output.window().expect("window").expect("a window");
            w.bytes()[..record.len()].copy_from_slice(record);
            w.put(0, record.len() as u64)
                .map_err(|(e, _)| e)
                .expect("put");
        });
        let mut put = None;
        let mut calls = 0;
        let mut released = 0;
        while put.is_none() || calls < EACH || released < 2 * EACH {
            match p.next(sys::now() + 5_000_000_000).expect("next") {
                Some(Event::Call {
                    call,
                    port,
                    request,
                }) => {
                    assert_eq!(request, port.to_le_bytes());
                    p.answer(call, Ok(&(port * 2).to_le_bytes()))
                        .expect("answer");
                    calls += 1;
                }
                Some(Event::Put { batch, .. }) => put = Some(batch),
                Some(Event::Released { .. }) => released += 1,
                Some(Event::Freed { .. }) => {}
                other => panic!("{other:?}"),
            }
        }
        for r in readers {
            assert_eq!(r.join().expect("a reader"), EACH);
        }
        caller.join().expect("the caller");
        writer.join().expect("the writer");
        let mine = p.map(put.expect("put")).expect("map");
        assert_eq!(mine.bytes(), record);
    });
    drop(plane);
    let rest = pump(&mut p);
    assert!(rest.contains(&Event::Ended { task }), "{rest:?}");
    assert_eq!(p.residency().shares, 0);
}
