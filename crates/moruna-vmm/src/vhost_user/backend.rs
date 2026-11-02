//! The vsock device as a vhost-user back end (Linux): QEMU's `vhost-user-vsock-pci` is the
//! front end, this process the device. QEMU shares the guest's memory (a `memfd` backend) and
//! hands over each ring's addresses and eventfds; this process runs the same
//! [`VirtioVsock`] and muxer `moruna-vmm` itself runs, so the host side is the muxer's Unix
//! sockets: a host process connects to `uds` and writes `CONNECT <port>\n` to reach a guest
//! port, and a guest connection to host port `P` reaches `<uds>_P`.
//!
//! QEMU keeps the event queue itself; the back end has the receive queue (0) and the
//! transmit queue (1). One thread serves everything: the front end's messages, the rings'
//! kicks, and the muxer's sockets, in one `poll`.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use virtio_queue::{Queue, QueueT};
use vm_memory::{FileOffset, GuestAddress};

use super::protocol::{self as p, Header, Region, req};
use crate::devices::Mem;
use crate::devices::virtio::{F_VERSION_1, Interrupt, QUEUE_MAX_SIZE, VirtioDevice};
use crate::devices::vsock::{RXQ, TXQ, VirtioVsock};
use crate::error::{Result, VmmError};
use crate::sys::{PollFd, poll};

/// How long one `poll` waits with nothing ready.
const POLL: Duration = Duration::from_millis(100);

fn err(what: impl Into<String>) -> VmmError {
    VmmError::device("vhost-user-vsock", what.into())
}

/// The interrupt the device raises: the rings' call eventfds.
struct Calls([Option<File>; 2]);

impl Calls {
    fn signal(&self) -> Result<()> {
        for f in self.0.iter().flatten() {
            (&*f)
                .write_all(&1u64.to_le_bytes())
                .map_err(|e| VmmError::io("write", "a call eventfd", &e))?;
        }
        Ok(())
    }
}

/// The interrupt `activate` is given; the device signals through [`Calls`] instead, and this
/// one is never raised.
struct NoIrq;

impl crate::devices::Irq for NoIrq {
    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One front end's session.
struct Session {
    conn: UnixStream,
    device: VirtioVsock,
    queues: Vec<Queue>,
    kicks: [Option<File>; 2],
    calls: Calls,
    enabled: [bool; 2],
    started: [bool; 2],
    regions: Vec<Region>,
    mem: Option<Mem>,
    acked: u64,
    protocol: bool,
    active: bool,
}

/// One message from the front end: its header, the descriptors sent with it, its payload.
struct Message {
    header: Header,
    fds: Vec<OwnedFd>,
    payload: Vec<u8>,
}

/// Reads one message; `None` once the front end has gone.
fn recv(conn: &UnixStream) -> Result<Option<Message>> {
    let mut head = [0u8; p::HEADER_BYTES];
    let mut cbuf = [0u64; 32];
    let mut iov = libc::iovec {
        iov_base: head.as_mut_ptr().cast(),
        iov_len: head.len(),
    };
    // SAFETY: `msghdr` is a plain C struct for which all zeroes is a valid value; every
    // pointer set below names a live buffer of the length given, which outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: `msg` describes the buffers above; the descriptors the kernel installs are
    // taken into `OwnedFd`s below, so none is leaked.
    let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(VmmError::io(
            "recvmsg",
            "the vhost-user socket",
            &std::io::Error::last_os_error(),
        ));
    }
    if n == 0 {
        return Ok(None);
    }
    let mut fds = Vec::new();
    // SAFETY: the control buffer was filled by `recvmsg`; the macros walk it within
    // `msg_controllen`, and each `SCM_RIGHTS` entry holds whole `c_int`s.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(err(
            "a message carried more descriptors than any request has",
        ));
    }
    if (n as usize) < head.len() {
        (&*conn)
            .read_exact(&mut head[n as usize..])
            .map_err(|e| VmmError::io("read", "a vhost-user header", &e))?;
    }
    let h = Header::decode(&head).map_err(|m| err(m.0))?;
    let mut payload = vec![0u8; h.size as usize];
    (&*conn)
        .read_exact(&mut payload)
        .map_err(|e| VmmError::io("read", "a vhost-user payload", &e))?;
    Ok(Some(Message {
        header: h,
        fds,
        payload,
    }))
}

impl Session {
    fn send(&mut self, request: u32, payload: &[u8]) -> Result<()> {
        self.conn
            .write_all(&p::reply(request, payload))
            .map_err(|e| VmmError::io("write", "a vhost-user reply", &e))
    }

    fn queue(&mut self, index: u32) -> Result<usize> {
        let i = index as usize;
        if i >= self.queues.len() {
            return Err(err(format!(
                "vring {index}: the back end has rings 0 and 1"
            )));
        }
        Ok(i)
    }

    fn one_fd(fds: Vec<OwnedFd>, what: &str) -> Result<Option<File>> {
        match fds.len() {
            0 => Ok(None),
            1 => Ok(fds.into_iter().next().map(File::from)),
            n => Err(err(format!("{what} with {n} descriptors"))),
        }
    }

    /// Serves one message; `false` once the front end has gone.
    fn serve(&mut self) -> Result<bool> {
        let Some(Message {
            header: h,
            fds,
            payload,
        }) = recv(&self.conn)?
        else {
            return Ok(false);
        };
        let malformed = |m: p::Malformed| err(format!("request {}: {}", h.request, m.0));
        match h.request {
            req::GET_FEATURES => {
                let f = F_VERSION_1 | self.device.features() | p::F_PROTOCOL_FEATURES;
                self.send(h.request, &f.to_le_bytes())?;
            }
            req::SET_FEATURES => {
                self.acked = p::u64_payload(&payload).map_err(malformed)?;
                self.protocol = self.acked & p::F_PROTOCOL_FEATURES != 0;
            }
            req::GET_PROTOCOL_FEATURES => self.send(h.request, &p::PF_CONFIG.to_le_bytes())?,
            req::SET_PROTOCOL_FEATURES => {
                let f = p::u64_payload(&payload).map_err(malformed)?;
                if f & !p::PF_CONFIG != 0 {
                    return Err(err(format!(
                        "protocol features {f:#x} beyond the {:#x} offered",
                        p::PF_CONFIG
                    )));
                }
            }
            req::GET_QUEUE_NUM => self.send(h.request, &2u64.to_le_bytes())?,
            req::SET_OWNER | req::RESET_OWNER => {}
            req::SET_MEM_TABLE => {
                let regions = p::mem_table(&payload, fds.len()).map_err(malformed)?;
                let mut ranges: Vec<(GuestAddress, usize, Option<FileOffset>)> = regions
                    .iter()
                    .zip(fds)
                    .map(|(r, fd)| {
                        (
                            GuestAddress(r.guest),
                            r.size as usize,
                            Some(FileOffset::new(File::from(fd), r.offset)),
                        )
                    })
                    .collect();
                ranges.sort_by_key(|r| r.0);
                let mem = Mem::from_ranges_with_files(ranges)
                    .map_err(|e| err(format!("mapping the guest's memory: {e}")))?;
                self.regions = regions;
                self.mem = Some(mem);
            }
            req::SET_VRING_NUM => {
                let (index, num) = p::vring_state(&payload).map_err(malformed)?;
                let i = self.queue(index)?;
                if num == 0 || num > u32::from(QUEUE_MAX_SIZE) || !num.is_power_of_two() {
                    return Err(err(format!("vring {index} of {num} entries")));
                }
                self.queues[i].set_size(num as u16);
            }
            req::SET_VRING_ADDR => {
                let a = p::vring_addr(&payload).map_err(malformed)?;
                let i = self.queue(a.index)?;
                let at = |user: u64, what: &str| {
                    p::translate(&self.regions, user).ok_or_else(|| {
                        err(format!(
                            "vring {}: its {what} at {user:#x} is in no shared region",
                            a.index
                        ))
                    })
                };
                let (desc, avail, used) = (
                    at(a.desc, "descriptors")?,
                    at(a.avail, "driver ring")?,
                    at(a.used, "device ring")?,
                );
                let q = &mut self.queues[i];
                q.set_desc_table_address(Some(desc as u32), Some((desc >> 32) as u32));
                q.set_avail_ring_address(Some(avail as u32), Some((avail >> 32) as u32));
                q.set_used_ring_address(Some(used as u32), Some((used >> 32) as u32));
            }
            req::SET_VRING_BASE => {
                let (index, num) = p::vring_state(&payload).map_err(malformed)?;
                let i = self.queue(index)?;
                self.queues[i].set_next_avail(num as u16);
                self.queues[i].set_next_used(num as u16);
            }
            req::GET_VRING_BASE => {
                let (index, _) = p::vring_state(&payload).map_err(malformed)?;
                let i = self.queue(index)?;
                self.queues[i].set_ready(false);
                self.started[i] = false;
                self.active = false;
                let next = u32::from(self.queues[i].next_avail());
                self.send(h.request, &p::vring_state_bytes(index, next))?;
            }
            req::SET_VRING_KICK | req::SET_VRING_CALL | req::SET_VRING_ERR => {
                let word = p::u64_payload(&payload).map_err(malformed)?;
                let i = self.queue((word & 0xff) as u32)?;
                let fd = if word & p::VRING_NOFD != 0 {
                    None
                } else {
                    Session::one_fd(fds, "a vring eventfd")?
                };
                match h.request {
                    req::SET_VRING_KICK => {
                        self.kicks[i] = fd;
                        self.started[i] = true;
                        self.queues[i].set_ready(true);
                    }
                    req::SET_VRING_CALL => self.calls.0[i] = fd,
                    _ => {}
                }
            }
            req::SET_VRING_ENABLE => {
                let (index, num) = p::vring_state(&payload).map_err(malformed)?;
                let i = self.queue(index)?;
                self.enabled[i] = num == 1;
            }
            req::GET_CONFIG => {
                let (offset, size, flags) = p::config_request(&payload).map_err(malformed)?;
                let mut bytes = vec![0u8; size as usize];
                self.device.read_config(u64::from(offset), &mut bytes);
                self.send(h.request, &p::config_reply(offset, flags, &bytes))?;
            }
            req::SET_CONFIG => {}
            other => {
                return Err(err(format!(
                    "request {other}, which this back end does not take"
                )));
            }
        }
        if h.flags & p::NEED_REPLY != 0
            && !matches!(
                h.request,
                req::GET_FEATURES
                    | req::GET_PROTOCOL_FEATURES
                    | req::GET_QUEUE_NUM
                    | req::GET_VRING_BASE
                    | req::GET_CONFIG
            )
        {
            self.send(h.request, &0u64.to_le_bytes())?;
        }
        self.activate()?;
        Ok(true)
    }

    /// Both rings started (and enabled, when protocol features were negotiated) over shared
    /// memory: the device runs.
    fn activate(&mut self) -> Result<()> {
        let ready = self.mem.is_some()
            && (0..2).all(|i| self.started[i] && (!self.protocol || self.enabled[i]));
        if ready && !self.active {
            self.active = true;
            self.device
                .activate(self.acked, Interrupt::new(Arc::new(NoIrq)))?;
            self.run(RXQ)?;
        }
        Ok(())
    }

    fn run(&mut self, index: usize) -> Result<()> {
        let Some(mem) = &self.mem else {
            return Ok(());
        };
        if self.device.process_queue(index, &mut self.queues, mem)? {
            self.calls.signal()?;
        }
        Ok(())
    }
}

/// Serves the one front end that connects at `socket` until it goes: the guest is `cid`, its
/// host side the muxer's sockets at `uds`.
pub fn serve(socket: &Path, cid: u32, uds: &Path) -> Result<()> {
    let listener = UnixListener::bind(socket)
        .map_err(|e| VmmError::io("bind", socket.display().to_string(), &e))?;
    let (conn, _) = listener
        .accept()
        .map_err(|e| VmmError::io("accept", "the vhost-user socket", &e))?;
    let queues = (0..2)
        .map(|_| Queue::new(QUEUE_MAX_SIZE).map_err(|e| err(format!("a queue: {e}"))))
        .collect::<Result<Vec<_>>>()?;
    let mut s = Session {
        conn,
        device: VirtioVsock::new(cid, uds)?,
        queues,
        kicks: [None, None],
        calls: Calls([None, None]),
        enabled: [false, false],
        started: [false, false],
        regions: Vec::new(),
        mem: None,
        acked: 0,
        protocol: false,
        active: false,
    };
    loop {
        let mut fds = vec![PollFd {
            fd: s.conn.as_raw_fd(),
            read: true,
            write: false,
        }];
        let kicks: Vec<usize> = (0..2).filter(|i| s.kicks[*i].is_some()).collect();
        fds.extend(kicks.iter().map(|i| PollFd {
            fd: s.kicks[*i].as_ref().map_or(-1, |f| f.as_raw_fd()),
            read: true,
            write: false,
        }));
        let set = if s.active {
            s.device.poll_set()
        } else {
            Vec::new()
        };
        fds.extend(set.iter().map(|(f, _)| *f));
        let ready = poll(&fds, POLL).map_err(|e| VmmError::io("poll", "vhost-user-vsock", &e))?;
        if ready[0].read || ready[0].error {
            if !s.serve()? {
                return Ok(());
            }
            continue;
        }
        for (k, i) in kicks.iter().enumerate() {
            if ready[1 + k].read {
                let mut word = [0u8; 8];
                if let Some(f) = &s.kicks[*i] {
                    (&*f)
                        .read_exact(&mut word)
                        .map_err(|e| VmmError::io("read", "a kick eventfd", &e))?;
                }
                if s.active {
                    s.run(if *i == RXQ { RXQ } else { TXQ })?;
                }
            }
        }
        let base = 1 + kicks.len();
        let pairs: Vec<_> = set
            .iter()
            .map(|(_, t)| *t)
            .zip(ready[base..].iter().copied())
            .collect();
        if s.active && pairs.iter().any(|(_, r)| r.read || r.write || r.error) {
            let Some(mem) = &s.mem else { continue };
            if s.device.backend_ready(&pairs, &mut s.queues, mem)? {
                s.calls.signal()?;
            }
        }
    }
}
