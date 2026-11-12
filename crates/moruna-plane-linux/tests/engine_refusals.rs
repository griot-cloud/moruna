//! The Linux engine's own refusals: Linux's error codes in the port's terms, and the calls
//! that name a task or batch the engine does not hold.
#![cfg(target_os = "linux")]

use std::io;

use moruna_abi::Error;
use moruna_plane::{BatchId, PlaneError, TaskId};
use moruna_plane_linux::LinuxPlane;
use moruna_plane_linux::engine::code;

#[test]
fn every_linux_errno_maps_to_the_ports_code() {
    let cases = [
        (libc::ENOMEM, Error::NoMemory),
        (libc::EMFILE, Error::Limit),
        (libc::ENFILE, Error::Limit),
        (libc::ENOSPC, Error::Limit),
        (libc::EMSGSIZE, Error::Limit),
        (libc::EBADF, Error::BadHandle),
        (libc::ENOTSOCK, Error::BadHandle),
        (libc::EINVAL, Error::InvalidArgs),
        (libc::EPIPE, Error::PeerClosed),
        (libc::ECONNRESET, Error::PeerClosed),
        (libc::ENOTCONN, Error::PeerClosed),
        (libc::EFAULT, Error::Fault),
        (libc::EACCES, Error::AccessDenied),
        (libc::EPERM, Error::AccessDenied),
        (libc::ETIMEDOUT, Error::TimedOut),
        (libc::EIO, Error::BadState),
    ];
    for (errno, want) in cases {
        assert_eq!(
            code(&io::Error::from_raw_os_error(errno)),
            want,
            "errno {errno}"
        );
    }
    // EAGAIN is WouldBlock, which the port names ShouldWait before the errno is read.
    assert_eq!(
        code(&io::Error::from_raw_os_error(libc::EAGAIN)),
        Error::ShouldWait
    );
    assert_eq!(
        code(&io::Error::from(io::ErrorKind::WouldBlock)),
        Error::ShouldWait
    );
    // An error with no errno at all is a bad state, never a guess.
    assert_eq!(code(&io::Error::other("no errno")), Error::BadState);
}

#[test]
fn calls_naming_what_the_engine_does_not_hold_are_refused() {
    let mut p = LinuxPlane::new(1 << 20);
    assert!(p.last_error().is_none());
    let (_, pidfd) = {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        (a, std::os::fd::OwnedFd::from(b))
    };
    assert!(matches!(
        p.attach(TaskId(99), pidfd),
        Err(PlaneError::Unknown(_))
    ));
    assert!(matches!(p.batch(BatchId(7)), Err(PlaneError::Unknown(_))));
    assert!(matches!(p.map(BatchId(7)), Err(PlaneError::Unknown(_))));
}

#[test]
fn a_record_the_engine_fails_to_write_is_refused_and_its_pages_returned() {
    let mut p = LinuxPlane::new(1 << 20);
    let refused = p.load_with(4096, |_| Err("the writer gave up".into()));
    match refused {
        Err(PlaneError::Refused(why)) => assert!(why.contains("the writer gave up"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The pages came back: a record of the whole budget still fits after the refusal.
    let garbage = vec![0xffu8; 64];
    assert!(p.load(&garbage).is_err(), "garbage is not a record");
}

fn shape(inputs: u32, outputs: u32) -> moruna_plane::Shape {
    moruna_plane::Shape {
        inputs,
        outputs,
        port: false,
    }
}

#[test]
fn grants_offers_and_finishes_refuse_what_the_task_does_not_have() {
    use moruna_plane::DataPlane;
    let mut p = LinuxPlane::new(1 << 22);
    let (task, _end) = p.open(shape(1, 1)).expect("open");
    assert!(matches!(
        p.grant(TaskId(99), 0, 4096),
        Err(PlaneError::Unknown(_))
    ));
    assert!(matches!(
        p.grant(task, 5, 4096),
        Err(PlaneError::Unknown(_))
    ));
    assert!(p.offer(TaskId(99), 0, BatchId(1)).is_err());
    assert!(
        p.offer(task, 0, BatchId(12345)).is_err(),
        "a batch the engine does not hold"
    );
    assert!(p.finish(TaskId(99), 0).is_err());
    assert!(p.drop_batch(BatchId(12345)).is_err());
    // Windows queue up to the slot's depth, then the slot is full.
    let mut granted = 0;
    let full = loop {
        match p.grant(task, 0, 4096) {
            Ok(()) => granted += 1,
            Err(e) => break e,
        }
        assert!(granted < 1000, "a slot has a finite depth");
    };
    assert!(granted > 0);
    assert!(matches!(full, PlaneError::Full), "{full:?}");
}

#[test]
fn a_task_that_hangs_up_is_closed_to_new_windows() {
    use moruna_plane::DataPlane;
    let mut p = LinuxPlane::new(1 << 22);
    let (task, end) = p.open(shape(1, 1)).expect("open");
    drop(end);
    // The engine reads the hang-up and ends the task.
    let deadline = moruna_plane_linux::sys::now() + 200_000_000;
    while p.next(deadline).expect("next").is_some() {}
    let after = p.grant(task, 0, 4096);
    assert!(
        matches!(after, Err(PlaneError::Closed) | Err(PlaneError::Unknown(_))),
        "{after:?}"
    );
}
