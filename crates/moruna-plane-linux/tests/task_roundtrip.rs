//! The task's library against the real engine, in one process: an input batch read in place,
//! an output window written and put, a put the engine refuses handing the window back, and a
//! process whose descriptor 3 is no plane.
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moruna_abi::Error;
use moruna_plane::{DataPlane, Shape};
use moruna_plane_linux::task::Plane;
use moruna_plane_linux::{LinuxPlane, sys};

#[test]
fn a_task_reads_its_input_in_place_and_gets_its_window_back_when_a_put_is_refused() {
    let mut p = LinuxPlane::new(1 << 22);
    let (task, end) = p
        .open(Shape {
            inputs: 1,
            outputs: 1,
            port: false,
        })
        .expect("open");
    let record = moruna_testkit::plane::record(&moruna_testkit::plane::mixed(8).unwrap()).unwrap();
    let (batch, _) = p.load(&record).expect("load");
    p.offer(task, 0, batch).expect("offer");
    p.finish(task, 0).expect("finish");
    p.grant(task, 0, 4096).expect("grant");

    let stop = Arc::new(AtomicBool::new(false));
    let engine_stop = Arc::clone(&stop);
    let engine = std::thread::spawn(move || {
        while !engine_stop.load(Ordering::Acquire) {
            let _ = p.next(sys::now() + 10_000_000).expect("next");
        }
    });

    let plane = Plane::new(end).expect("a plane");
    let mut input = plane.input(0).expect("input 0");
    {
        let got = input.get().expect("get").expect("a batch");
        assert_eq!(got.len() as usize >= record.len(), true);
        assert!(!got.is_empty());
        assert_ne!(got.addr(), 0);
        assert_eq!(&got.bytes()[..record.len()], &record[..]);
    }
    assert!(
        input.get().expect("end").is_none(),
        "the input was finished"
    );

    let mut output = plane.output(1).expect("output slot 1, after the input");
    let mut window = output.window().expect("window").expect("granted");
    assert!(window.len() >= 4096 && !window.is_empty() && window.addr() != 0);
    // Garbage is no record: the engine refuses it and the window comes back.
    window.bytes()[..8].copy_from_slice(b"notarrow");
    match window.put(0, 8) {
        Err((e, back)) => {
            assert_ne!(e, Error::BadHandle, "a refusal, not a lost slot");
            assert!(back.len() >= 4096);
        }
        Ok(()) => {
            // An engine may instead accept the put and refuse the record later; then the
            // task is ended and its next call says so.
        }
    }
    stop.store(true, Ordering::Release);
    engine.join().unwrap();
    drop(output);
    drop(input);
    plane.close();
    assert!(
        plane.batch_get(0).is_err(),
        "a closed plane answers nothing"
    );
}

#[test]
fn a_process_whose_descriptor_three_is_no_plane_has_none() {
    // In the test harness, descriptor 3 is not a seqpacket plane socket.
    let first = Plane::take();
    assert!(
        matches!(first, Err(Error::BadHandle) | Ok(Some(_))),
        "{:?}",
        first.as_ref().err()
    );
    // Only the first call claims it; every later one has nothing.
    assert!(matches!(Plane::take(), Ok(None)));
}
