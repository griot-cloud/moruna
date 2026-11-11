//! The task's library against a scripted engine end: each answer the engine may give a
//! `batch_get`, including the malformed ones, and a plane that is not a plane socket.
#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use moruna_abi::Error;
use moruna_plane_linux::sys;
use moruna_plane_linux::task::Plane;
use moruna_plane_linux::wire::{self, Header};

fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: socketpair writes two new descriptors into `fds` on success.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0);
    // SAFETY: both descriptors were just created and are owned by nothing else.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// A task calling `batch_get(0)` while the engine answers with `answer`.
fn get_answered_with(answer: Option<Header>) -> Result<bool, Error> {
    let (task, engine) = seqpacket_pair();
    let plane = Plane::new(task).unwrap();
    let engine_side = std::thread::spawn(move || {
        let mut buf = [0u8; wire::HEADER];
        let got = sys::recv(engine.as_fd(), &mut buf, true).unwrap();
        let request = Header::read(&buf[..got.len]).unwrap();
        assert_eq!(request.kind, wire::GET);
        match answer {
            Some(mut h) => {
                h.slot = request.slot;
                sys::send(engine.as_fd(), &h.bytes(), None, true).unwrap();
                engine
            }
            // No answer: the engine hangs up.
            None => {
                drop(engine);
                let (a, _) = seqpacket_pair();
                a
            }
        }
    });
    let result = plane.batch_get(0).map(|g| g.is_some());
    drop(engine_side.join().unwrap());
    result
}

#[test]
fn a_plane_must_be_a_seqpacket_socket() {
    let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
    assert_eq!(Plane::new(OwnedFd::from(a)).err(), Some(Error::BadHandle));
}

#[test]
fn end_means_no_more_batches() {
    assert_eq!(
        get_answered_with(Some(Header::new(wire::END, 0))),
        Ok(false)
    );
}

#[test]
fn an_error_answer_is_the_engines_error() {
    let refused = Header::error(wire::GET, 0, Error::AccessDenied);
    assert_eq!(get_answered_with(Some(refused)), Err(Error::AccessDenied));
}

#[test]
fn a_batch_or_window_without_its_memfd_is_a_bad_state() {
    for kind in [wire::BATCH, wire::WINDOW] {
        let mut h = Header::new(kind, 0);
        h.b = 4096;
        assert_eq!(
            get_answered_with(Some(h)),
            Err(Error::BadState),
            "kind {kind:#x}"
        );
    }
}

#[test]
fn an_unknown_answer_is_a_bad_state() {
    assert_eq!(
        get_answered_with(Some(Header::new(0x7f, 0))),
        Err(Error::BadState)
    );
}

#[test]
fn a_hung_up_engine_is_peer_closed() {
    assert_eq!(get_answered_with(None), Err(Error::PeerClosed));
}

#[test]
fn closing_the_plane_ends_every_later_call() {
    let (task, _engine) = seqpacket_pair();
    let plane = Plane::new(task).unwrap();
    plane.close();
    assert_eq!(plane.batch_get(0).err(), Some(Error::PeerClosed));
    assert_eq!(plane.batch_put(0, 0, 0).err(), Some(Error::PeerClosed));
}

#[test]
fn slots_and_the_port_are_each_given_once() {
    let (task, _engine) = seqpacket_pair();
    let plane = Plane::new(task).unwrap();
    assert!(plane.input(0).is_some());
    assert!(plane.input(0).is_none());
    assert!(plane.output(1).is_some());
    assert!(plane.output(1).is_none());
    assert!(plane.port().is_some());
    assert!(plane.port().is_none());
}

#[test]
fn headers_round_trip_and_short_messages_are_not_headers() {
    let mut h = Header::new(wire::PUT, 3);
    h.a = 1;
    h.b = 2;
    h.c = 3;
    assert_eq!(Header::read(&h.bytes()), Some(h));
    assert_eq!(Header::read(&h.bytes()[..wire::HEADER - 1]), None);
    assert_eq!(Header::new(wire::BATCH, 0).status(), None);
    let mut ok = Header::new(wire::REPLY, 0);
    ok.b = 0;
    assert_eq!(ok.status(), None);
}
