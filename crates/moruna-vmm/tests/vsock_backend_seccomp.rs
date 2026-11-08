//! The real `moruna-vhost-vsock` under its own system-call filter, driven by a vhost-user front
//! end through negotiation, shared memory, both rings, activation, a host connection and the
//! front end's departure: it must serve all of it and exit 0, never die by `SIGSYS`.
#![cfg(target_os = "linux")]

use moruna_vmm::vhost_user::protocol::{self as p, req};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{fs::DirBuilderExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn send(conn: &UnixStream, request: u32, payload: &[u8], fds: &[RawFd]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&request.to_le_bytes());
    bytes.extend_from_slice(&p::VERSION.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0u64; 8];
    // SAFETY: msghdr over live buffers; CMSG macros stay within `control`.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let space = libc::CMSG_SPACE((fds.len() * 4) as u32) as usize;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&raw const msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(c).cast::<RawFd>(),
                fds.len(),
            );
        }
        assert_eq!(
            libc::sendmsg(conn.as_raw_fd(), &raw const msg, 0),
            bytes.len() as isize
        );
    }
}

fn reply(conn: &mut UnixStream, request: u32) -> Vec<u8> {
    let mut head = [0u8; p::HEADER_BYTES];
    conn.read_exact(&mut head).unwrap();
    let h = p::Header::decode(&head).unwrap();
    assert_eq!(h.request, request);
    assert_ne!(h.flags & p::REPLY, 0);
    let mut payload = vec![0u8; h.size as usize];
    conn.read_exact(&mut payload).unwrap();
    payload
}

fn state(index: u32, num: u32) -> Vec<u8> {
    p::vring_state_bytes(index, num).to_vec()
}

fn eventfd() -> OwnedFd {
    // SAFETY: a new descriptor we own.
    unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_CLOEXEC)) }
}

fn connect_when_listening(path: &Path, backend: &mut Child) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // bind creates the socket path before listen makes it connectable. Only
        // an actual connection proves readiness, including for the guest muxer.
        match UnixStream::connect(path) {
            Ok(conn) => return conn,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                assert!(
                    backend.try_wait().unwrap().is_none(),
                    "back end exited before listening on {}: {e}",
                    path.display()
                );
                assert!(
                    Instant::now() < deadline,
                    "back end never listened on {}: {e}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("connecting to {}: {e}", path.display()),
        }
    }
}

#[test]
fn backend_serves_a_whole_session_inside_its_filter() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moruna-vsock-filter-{}-{nanos}",
        std::process::id()
    ));
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let scratch = Scratch(dir);
    let socket = scratch.0.join("vhost.sock");
    let uds = scratch.0.join("guest.uds");
    let mut backend = Command::new(env!("CARGO_BIN_EXE_moruna-vhost-vsock"))
        .arg("--socket")
        .arg(&socket)
        .arg("--cid")
        .arg("3")
        .arg("--uds")
        .arg(&uds)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut conn = connect_when_listening(&socket, &mut backend);
    let status = fs::read_to_string(format!("/proc/{}/status", backend.id())).unwrap();
    assert!(
        status.lines().any(|l| l == "Seccomp:\t2"),
        "not filtered:\n{status}"
    );
    assert!(status.lines().any(|l| l == "NoNewPrivs:\t1"));

    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    send(&conn, req::GET_FEATURES, &[], &[]);
    let features = p::u64_payload(&reply(&mut conn, req::GET_FEATURES)).unwrap();
    assert_ne!(features & p::F_PROTOCOL_FEATURES, 0);
    send(&conn, req::SET_FEATURES, &features.to_le_bytes(), &[]);
    send(&conn, req::GET_PROTOCOL_FEATURES, &[], &[]);
    reply(&mut conn, req::GET_PROTOCOL_FEATURES);
    send(
        &conn,
        req::SET_PROTOCOL_FEATURES,
        &p::PF_CONFIG.to_le_bytes(),
        &[],
    );
    send(&conn, req::SET_OWNER, &[], &[]);
    send(&conn, req::GET_QUEUE_NUM, &[], &[]);
    reply(&mut conn, req::GET_QUEUE_NUM);
    let mut config = Vec::new();
    for word in [0u32, 8, 0] {
        config.extend_from_slice(&word.to_le_bytes());
    }
    config.extend_from_slice(&[0u8; 8]);
    send(&conn, req::GET_CONFIG, &config, &[]);
    let cid = reply(&mut conn, req::GET_CONFIG);
    assert_eq!(u64::from_le_bytes(cid[12..20].try_into().unwrap()), 3);

    // One megabyte of shared guest memory, as QEMU's memfd backend shares it.
    const SIZE: u64 = 1 << 20;
    const USER: u64 = 0x7000_0000_0000;
    // SAFETY: a new descriptor we own, sized below.
    let memory =
        unsafe { OwnedFd::from_raw_fd(libc::memfd_create(c"guest".as_ptr(), libc::MFD_CLOEXEC)) };
    // SAFETY: ftruncate on our own descriptor.
    assert_eq!(
        unsafe { libc::ftruncate(memory.as_raw_fd(), SIZE as i64) },
        0
    );
    let mut table = Vec::new();
    table.extend_from_slice(&1u32.to_le_bytes());
    table.extend_from_slice(&0u32.to_le_bytes());
    for word in [0u64, SIZE, USER, 0] {
        table.extend_from_slice(&word.to_le_bytes());
    }
    send(&conn, req::SET_MEM_TABLE, &table, &[memory.as_raw_fd()]);
    let mut kicks = Vec::new();
    let mut calls = Vec::new();
    for ring in 0..2u32 {
        let base = USER + u64::from(ring) * 0x10000;
        send(&conn, req::SET_VRING_NUM, &state(ring, 64), &[]);
        let mut addr = Vec::new();
        addr.extend_from_slice(&ring.to_le_bytes());
        addr.extend_from_slice(&0u32.to_le_bytes());
        for word in [base, base + 0x2000, base + 0x1000, 0] {
            addr.extend_from_slice(&word.to_le_bytes());
        }
        send(&conn, req::SET_VRING_ADDR, &addr, &[]);
        send(&conn, req::SET_VRING_BASE, &state(ring, 0), &[]);
        let call = eventfd();
        send(
            &conn,
            req::SET_VRING_CALL,
            &u64::from(ring).to_le_bytes(),
            &[call.as_raw_fd()],
        );
        let kick = eventfd();
        send(
            &conn,
            req::SET_VRING_KICK,
            &u64::from(ring).to_le_bytes(),
            &[kick.as_raw_fd()],
        );
        send(&conn, req::SET_VRING_ENABLE, &state(ring, 1), &[]);
        kicks.push(kick);
        calls.push(call);
    }
    // The device is active: its muxer listens for the host. A host connection asks for a guest
    // port; the guest has posted no buffers, so it waits.
    let mut host = connect_when_listening(&uds, &mut backend);
    host.write_all(b"CONNECT 1234\n").unwrap();
    for kick in &kicks {
        // SAFETY: an eventfd write of one counter word.
        assert_eq!(
            unsafe { libc::write(kick.as_raw_fd(), 1u64.to_le_bytes().as_ptr().cast(), 8) },
            8
        );
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        backend.try_wait().unwrap().is_none(),
        "back end died serving"
    );
    send(&conn, req::GET_VRING_BASE, &state(0, 0), &[]);
    reply(&mut conn, req::GET_VRING_BASE);
    drop(host);
    drop(conn);
    let status = backend.wait().unwrap();
    let mut stderr = String::new();
    backend
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(0), "back end ended {status}: {stderr}");
}
