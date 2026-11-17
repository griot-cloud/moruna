//! The calls SDD 30 adds to the personality, for a program such as the engine: eventfd
//! and epoll with blocking waits answered later (f.4), rename in the scratch directory,
//! what its machine has (`sysinfo`, f.3), and a data-plane task's descriptors 0 and 1
//! answered later when their slot has nothing yet (f.10), so the server never waits inside
//! a request for the engine, which is one of its own programs.
//!
//! A call that would block is not answered when it arrives: the thread waits on its
//! forward while the server goes on serving every task. It is answered when a later call
//! of the same task makes it ready (a write to an eventfd it waits on), or, for a wait
//! with a timeout, when the deadline passes ([`Task::expire`], from the main loop).

use alloc::{rc::Rc, vec::Vec};

use imagefs_protocol::Bytes as FsBytes;
use moruna_linux::{
    events::{self, Epoll, EventFd, Seen, Step, ep},
    machine,
    table::nr,
    types::{errno, open},
};
use moruna_svc::println;

use crate::{
    files::{self, Open},
    memory,
    plane::Io,
    task::{Answer, Task},
};

/// What a blocked thread waits for.
pub enum Wait {
    /// `epoll_wait` on `epfd`: up to `max` events into the array at `events`.
    Epoll { epfd: u64, events: u64, max: usize },
    /// `read` of an eventfd into `buf`.
    Read { fd: u64, buf: u64, len: u64 },
    /// `write` of `value` to an eventfd.
    Write { fd: u64, len: u64, value: u64 },
    /// `read` of a plane task's descriptor 0, waiting for its next batch.
    PlaneRead { buf: u64, count: u64 },
    /// `write` of its descriptor 1, answered with `count` once what it held is put.
    PlaneWrite { count: u64 },
}

/// A thread whose call is answered later.
pub struct Blocked {
    pub token: u64,
    pub wait: Wait,
    /// Nanoseconds on the kernel's clock; `None` waits for ever.
    pub deadline: Option<u64>,
}

/// The identity of an open file description: what an epoll entry is for.
fn description(file: &Rc<core::cell::RefCell<Open>>) -> usize {
    Rc::as_ptr(file) as *const () as usize
}

impl Task {
    /// `eventfd2(initial, flags)`.
    pub fn eventfd(&mut self, initial: u64, flags: u64) -> Result<u64, u64> {
        let counter = EventFd::new(initial, flags)?;
        let file = Open::EventFd {
            counter,
            flags: open::O_RDWR | (flags & open::O_NONBLOCK),
        };
        self.fds
            .open(file, flags & events::efd::CLOEXEC != 0, 0)
            .map(|n| n as u64)
            .ok_or(errno::EMFILE)
    }

    /// `epoll_create1(flags)`.
    pub fn epoll_create(&mut self, flags: u64) -> Result<u64, u64> {
        if flags & !ep::CLOEXEC != 0 {
            return Err(errno::EINVAL);
        }
        self.fds
            .open(Open::Epoll(Epoll::new()), flags & ep::CLOEXEC != 0, 0)
            .map(|n| n as u64)
            .ok_or(errno::EMFILE)
    }

    /// `epoll_ctl(epfd, op, fd, event)`.
    pub fn epoll_ctl(&mut self, epfd: u64, op: u64, fd: u64, event: u64) -> Result<u64, u64> {
        let instance = self.fds.get(epfd).ok_or(errno::EBADF)?.clone();
        let target = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        if epfd == fd || Rc::ptr_eq(&instance.file, &target.file) {
            return Err(errno::EINVAL);
        }
        match &*target.file.borrow() {
            // Linux refuses a regular file or a directory: it is always ready.
            Open::Fs(_) | Open::Text { .. } => return Err(errno::EPERM),
            // An epoll instance inside another is Linux's; this personality has none.
            Open::Epoll(_) => {
                self.refused.count(nr::EPOLL_CTL);
                return Err(errno::EINVAL);
            }
            _ => {}
        }
        let wanted = if op == ep::CTL_DEL {
            None
        } else {
            let bytes = memory::read(&self.process, event, ep::EVENT_SIZE)?;
            Some(events::decode(&bytes).ok_or(errno::EFAULT)?)
        };
        let mut file = instance.file.borrow_mut();
        let Open::Epoll(e) = &mut *file else {
            return Err(errno::EINVAL);
        };
        e.ctl(op, fd, description(&target.file), wanted)?;
        Ok(0)
    }

    /// What descriptor `fd` is now, for an epoll instance's entries.
    fn seen(&self, fd: u64) -> Option<Seen> {
        let entry = self.fds.get(fd)?;
        let description = description(&entry.file);
        let file = entry.file.try_borrow().ok()?;
        let (ready, generation) = match &*file {
            Open::EventFd { counter, .. } => (counter.ready(), counter.generation()),
            // Streams and devices are ready for what they are opened for: a read or a
            // write of one is answered inside the call, as `poll` says.
            Open::Console(0) | Open::Plane(0) => (ep::IN | ep::RDNORM, 0),
            Open::Console(_) | Open::Plane(_) | Open::Log(_) => (ep::OUT | ep::WRNORM, 0),
            Open::Device(_) => (ep::IN | ep::RDNORM | ep::OUT | ep::WRNORM, 0),
            Open::Fs(_) | Open::Text { .. } | Open::Epoll(_) => (0, 0),
        };
        Some(Seen {
            description,
            ready,
            generation,
        })
    }

    /// The events epoll instance `epfd` has ready, at most `max`, written to `events`:
    /// how many, or `None` with none ready.
    fn collect(&mut self, epfd: u64, events_at: u64, max: usize) -> Result<Option<u64>, u64> {
        let instance = self.fds.get(epfd).ok_or(errno::EBADF)?.clone();
        let ready = {
            let mut file = instance.file.borrow_mut();
            let Open::Epoll(e) = &mut *file else {
                return Err(errno::EINVAL);
            };
            e.collect(max, |fd| self.seen(fd))
        };
        if ready.is_empty() {
            return Ok(None);
        }
        memory::write(&self.process, events_at, &events::encode(&ready))?;
        Ok(Some(ready.len() as u64))
    }

    /// `epoll_wait` and its forms: answered now when something is ready or the timeout
    /// is zero, otherwise when something is or the deadline passes.
    pub fn epoll_wait(
        &mut self,
        token: u64,
        epfd: u64,
        events_at: u64,
        max: u64,
        deadline: Option<u64>,
    ) -> Answer {
        if max == 0 || max > ep::MAX_EVENTS {
            return Answer::from(Err(errno::EINVAL));
        }
        let max = max as usize;
        match self.collect(epfd, events_at, max) {
            Ok(Some(n)) => Answer::Value(n),
            Ok(None) if deadline.is_some_and(|d| d <= moruna_svc::now()) => Answer::Value(0),
            Ok(None) => {
                self.blocked.push(Blocked {
                    token,
                    wait: Wait::Epoll {
                        epfd,
                        events: events_at,
                        max,
                    },
                    deadline,
                });
                Answer::Later
            }
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// `epoll_pwait2`'s timeout: a `struct timespec` at `at`, none when it is null.
    pub fn timespec_deadline(&self, at: u64) -> Result<Option<u64>, u64> {
        if at == 0 {
            return Ok(None);
        }
        let bytes = memory::read(&self.process, at, 16)?;
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[i..i + 8]);
            i64::from_le_bytes(w)
        };
        let (secs, nanos) = (word(0), word(8));
        if secs < 0 || !(0..1_000_000_000).contains(&nanos) {
            return Err(errno::EINVAL);
        }
        let span = (secs as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nanos as u64);
        Ok(Some(moruna_svc::now().saturating_add(span)))
    }

    /// Whether `fd` is an eventfd, and opened without `O_NONBLOCK`.
    pub fn eventfd_of(&self, fd: u64) -> Option<bool> {
        let entry = self.fds.get(fd)?;
        match &*entry.file.borrow() {
            Open::EventFd { flags, .. } => Some(flags & open::O_NONBLOCK == 0),
            _ => None,
        }
    }

    /// One read of an eventfd: its 8 bytes written to `buf`, or `WouldBlock`.
    fn eventfd_read(&mut self, fd: u64, buf: u64, len: u64) -> Result<Step<u64>, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let step = {
            let mut file = entry.file.borrow_mut();
            let Open::EventFd { counter, .. } = &mut *file else {
                return Err(errno::EINVAL);
            };
            counter.read(len)?
        };
        if let Step::Done(value) = step {
            memory::write(&self.process, buf, &value.to_le_bytes())?;
            return Ok(Step::Done(8));
        }
        Ok(Step::WouldBlock)
    }

    fn eventfd_write(&mut self, fd: u64, len: u64, value: u64) -> Result<Step<u64>, u64> {
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let mut file = entry.file.borrow_mut();
        let Open::EventFd { counter, .. } = &mut *file else {
            return Err(errno::EINVAL);
        };
        Ok(match counter.write(len, value)? {
            Step::Done(()) => Step::Done(8),
            Step::WouldBlock => Step::WouldBlock,
        })
    }

    /// `read` of an eventfd: at once, `EAGAIN` when empty and nonblocking, or later.
    pub fn read_eventfd(
        &mut self,
        token: u64,
        fd: u64,
        buf: u64,
        len: u64,
        blocks: bool,
    ) -> Answer {
        match self.eventfd_read(fd, buf, len) {
            Ok(Step::Done(n)) => Answer::Value(n),
            Ok(Step::WouldBlock) if !blocks => Answer::from(Err(errno::EAGAIN)),
            Ok(Step::WouldBlock) => {
                self.blocked.push(Blocked {
                    token,
                    wait: Wait::Read { fd, buf, len },
                    deadline: None,
                });
                Answer::Later
            }
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// `write` of an eventfd: its 8 bytes from `buf`.
    pub fn write_eventfd(
        &mut self,
        token: u64,
        fd: u64,
        buf: u64,
        len: u64,
        blocks: bool,
    ) -> Answer {
        if len < 8 {
            return Answer::from(Err(errno::EINVAL));
        }
        let value = match memory::read(&self.process, buf, 8) {
            Ok(bytes) => {
                let mut w = [0u8; 8];
                w.copy_from_slice(&bytes);
                u64::from_le_bytes(w)
            }
            Err(e) => return Answer::from(Err(e)),
        };
        match self.eventfd_write(fd, len, value) {
            Ok(Step::Done(n)) => Answer::Value(n),
            Ok(Step::WouldBlock) if !blocks => Answer::from(Err(errno::EAGAIN)),
            Ok(Step::WouldBlock) => {
                self.blocked.push(Blocked {
                    token,
                    wait: Wait::Write { fd, len, value },
                    deadline: None,
                });
                Answer::Later
            }
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// Answers every blocked call that can be answered now. A call answered may make
    /// another ready (a waiting writer frees room for nothing, a reader empties a
    /// counter an epoll watches), so it looks until nothing moves.
    pub fn wake(&mut self) {
        loop {
            let mut moved = false;
            let mut i = 0;
            while i < self.blocked.len() {
                let answer = match self.blocked[i].wait {
                    Wait::Epoll { epfd, events, max } => match self.collect(epfd, events, max) {
                        Ok(Some(n)) => Some(Answer::Value(n)),
                        Ok(None) => None,
                        Err(e) => Some(Answer::from(Err(e))),
                    },
                    Wait::Read { fd, buf, len } => match self.eventfd_read(fd, buf, len) {
                        Ok(Step::Done(n)) => Some(Answer::Value(n)),
                        Ok(Step::WouldBlock) => None,
                        Err(e) => Some(Answer::from(Err(e))),
                    },
                    Wait::Write { fd, len, value } => match self.eventfd_write(fd, len, value) {
                        Ok(Step::Done(n)) => Some(Answer::Value(n)),
                        Ok(Step::WouldBlock) => None,
                        Err(e) => Some(Answer::from(Err(e))),
                    },
                    Wait::PlaneRead { buf, count } => {
                        match self.plane_io(|p, proc| p.read(proc, buf, count)) {
                            Ok(Io::Done(n)) => Some(Answer::Value(n)),
                            Ok(Io::Waits) => None,
                            Err(e) => Some(Answer::from(Err(e))),
                        }
                    }
                    Wait::PlaneWrite { count } => match self.plane_io(|p, proc| {
                        p.flush(proc)
                            .map(|done| if done { Io::Done(count) } else { Io::Waits })
                    }) {
                        Ok(Io::Done(n)) => Some(Answer::Value(n)),
                        Ok(Io::Waits) => None,
                        Err(e) => Some(Answer::from(Err(e))),
                    },
                };
                match answer {
                    Some(answer) => {
                        let blocked = self.blocked.remove(i);
                        self.answer_later(blocked.token, answer);
                        moved = true;
                    }
                    None => i += 1,
                }
            }
            if !moved {
                return;
            }
        }
    }

    /// The plane descriptor `fd` is, 0 or 1, when it is one.
    pub fn plane_fd(&self, fd: u64) -> Option<u8> {
        let entry = self.fds.get(fd)?;
        match &*entry.file.borrow() {
            Open::Plane(s) => Some(*s),
            _ => None,
        }
    }

    fn plane_io(
        &mut self,
        f: impl FnOnce(&mut crate::plane::PlaneIo, &moruna_svc::Process) -> Result<Io, u64>,
    ) -> Result<Io, u64> {
        let plane = self.plane.as_mut().ok_or(errno::EBADF)?;
        f(plane, &self.process)
    }

    /// `read(0)` of a plane task: now, or when its next batch is pushed.
    pub fn read_plane(&mut self, token: u64, buf: u64, count: u64) -> Answer {
        let count = count.min(1 << 20);
        match self.plane_io(|p, proc| p.read(proc, buf, count)) {
            Ok(Io::Done(n)) => Answer::Value(n),
            Ok(Io::Waits) => {
                self.blocked.push(Blocked {
                    token,
                    wait: Wait::PlaneRead { buf, count },
                    deadline: None,
                });
                Answer::Later
            }
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// `write(1)` of a plane task: its bytes taken now, answered when they are put.
    pub fn write_plane(&mut self, token: u64, buf: u64, count: u64) -> Answer {
        let count = count.min(1 << 20);
        let data = match memory::read(&self.process, buf, count as usize) {
            Ok(d) => d,
            Err(e) => return Answer::from(Err(e)),
        };
        self.write_plane_bytes(token, &data)
    }

    fn write_plane_bytes(&mut self, token: u64, data: &[u8]) -> Answer {
        let count = data.len() as u64;
        // A write that comes while another waits for its window is answered after it.
        let behind = self
            .blocked
            .iter()
            .any(|b| matches!(b.wait, Wait::PlaneWrite { .. }));
        match self.plane_io(|p, proc| p.write(proc, data)) {
            Ok(Io::Done(n)) if !behind => Answer::Value(n),
            Ok(_) => {
                self.blocked.push(Blocked {
                    token,
                    wait: Wait::PlaneWrite { count },
                    deadline: None,
                });
                Answer::Later
            }
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// `readv(0)`: into its first buffer of any length, which a short read is.
    pub fn readv_plane(&mut self, token: u64, iov: u64, count: u64) -> Answer {
        match self.iovecs(iov, count) {
            Ok(list) => match list.into_iter().find(|(_, len)| *len > 0) {
                Some((base, len)) => self.read_plane(token, base, len),
                None => Answer::Value(0),
            },
            Err(e) => Answer::from(Err(e)),
        }
    }

    /// `writev(1)`: every buffer, as one write.
    pub fn writev_plane(&mut self, token: u64, iov: u64, count: u64) -> Answer {
        let list = match self.iovecs(iov, count) {
            Ok(l) => l,
            Err(e) => return Answer::from(Err(e)),
        };
        let mut data = Vec::new();
        for (base, len) in list {
            if data.len() as u64 + len > 1 << 20 {
                break;
            }
            match memory::read(&self.process, base, len as usize) {
                Ok(d) => data.extend_from_slice(&d),
                Err(e) => return Answer::from(Err(e)),
            }
        }
        self.write_plane_bytes(token, &data)
    }

    fn iovecs(&self, iov: u64, count: u64) -> Result<Vec<(u64, u64)>, u64> {
        if count > 1024 {
            return Err(errno::EINVAL);
        }
        let table = memory::read(&self.process, iov, count as usize * 16)?;
        (0..count as usize)
            .map(|i| moruna_linux::types::iovec(&table[i * 16..i * 16 + 16]).ok_or(errno::EFAULT))
            .collect()
    }

    /// Whether a call waits on the plane's input (`false`) or its output (`true`) slot.
    pub fn waits_on_plane(&self, output: bool) -> bool {
        self.blocked.iter().any(|b| match b.wait {
            Wait::PlaneRead { .. } => !output,
            Wait::PlaneWrite { .. } => output,
            _ => false,
        })
    }

    /// The task's slots are gone with it: a call still waiting on one is answered with
    /// `EIO`.
    pub fn plane_ended(&mut self) {
        let (gone, rest): (Vec<Blocked>, Vec<Blocked>) = core::mem::take(&mut self.blocked)
            .into_iter()
            .partition(|b| matches!(b.wait, Wait::PlaneRead { .. } | Wait::PlaneWrite { .. }));
        self.blocked = rest;
        for blocked in gone {
            self.answer_later(blocked.token, Answer::from(Err(errno::EIO)));
        }
    }

    /// Answers every blocked wait whose deadline is `now` or before with no events.
    pub fn expire(&mut self, now: u64) {
        let (due, rest): (Vec<Blocked>, Vec<Blocked>) = core::mem::take(&mut self.blocked)
            .into_iter()
            .partition(|b| b.deadline.is_some_and(|d| d <= now));
        self.blocked = rest;
        for blocked in due {
            self.answer_later(blocked.token, Answer::Value(0));
        }
    }

    /// The first deadline of a blocked call.
    pub fn next_deadline(&self) -> Option<u64> {
        self.blocked.iter().filter_map(|b| b.deadline).min()
    }

    /// `rename`, `renameat` and `renameat2` with `RENAME_NOREPLACE`; `RENAME_EXCHANGE`
    /// and `RENAME_WHITEOUT` are refused and counted.
    pub fn rename(
        &mut self,
        olddir: u64,
        old: u64,
        newdir: u64,
        new: u64,
        flags: u64,
    ) -> Result<u64, u64> {
        const RENAME_NOREPLACE: u64 = 1;
        if flags & !RENAME_NOREPLACE != 0 {
            self.refused.count(nr::RENAMEAT2);
            return Err(errno::EINVAL);
        }
        let from = self.resolve(olddir, old)?;
        let to = self.resolve(newdir, new)?;
        if flags & RENAME_NOREPLACE != 0 && self.fs.stat(FsBytes(to.clone()), false).is_ok() {
            return Err(errno::EEXIST);
        }
        self.fs
            .rename(FsBytes(from), FsBytes(to))
            .map_err(files::errno_of)?;
        Ok(0)
    }

    /// `sysinfo`: the account the program was granted, as the kernel has it now;
    /// refused without one.
    pub fn sysinfo(&mut self, at: u64) -> Result<u64, u64> {
        let Some(memory) = self.machine()? else {
            self.refused.count(nr::SYSINFO);
            return Err(errno::ENOSYS);
        };
        let info = machine::sysinfo(memory, moruna_svc::now());
        memory::write(&self.process, at, &info)?;
        Ok(0)
    }

    /// The program's memory, from the account it was granted (SDD 20 f.18): its limit,
    /// and what is left of it now; `None` when it was granted none. An account the
    /// kernel cannot read is `EIO` and the reason on the console.
    fn machine(&self) -> Result<Option<machine::Memory>, u64> {
        let Some(account) = &self.account else {
            return Ok(None);
        };
        let usage = account.usage().map_err(|e| {
            println!(
                self.shared.borrow().out,
                "linux: {}: its account: {}",
                self.name,
                e.name()
            );
            errno::EIO
        })?;
        Ok(Some(machine::Memory {
            total: usage.limit,
            free: usage.room(),
        }))
    }

    /// A file the server writes itself at `path`, for this program.
    pub fn text_file(&self, path: &[u8]) -> Result<Option<Open>, u64> {
        let cpus = self.shared.borrow().cpus;
        let memory = self.machine()?;
        Ok(machine::file(path, memory, cpus).map(|bytes| Open::Text { bytes, offset: 0 }))
    }
}
