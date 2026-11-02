//! `eventfd` and `epoll` (SDD 30 f.4): what an event loop such as tokio's (through mio)
//! needs of Linux when every descriptor it watches is one of its own counters. The server
//! keeps these as a task's open files; this module is their state and their rules, with
//! Linux's meaning:
//!
//! - an eventfd is a 64-bit counter: `write` adds (refused at `u64::MAX`, would block
//!   past `u64::MAX - 1`), `read` takes it whole (or one, with `EFD_SEMAPHORE`) and would
//!   block at zero; it is readable while nonzero and writable while another 1 fits;
//! - an epoll instance is an interest list of descriptors with the events asked for and
//!   the caller's data; level-triggered entries report while ready, edge-triggered ones
//!   (`EPOLLET`) once for each change, `EPOLLONESHOT` ones once until `EPOLL_CTL_MOD`;
//!   `EPOLLERR` and `EPOLLHUP` are always reported, never asked for.
//!
//! A change is a descriptor's generation moving: every `read` and `write` of an eventfd
//! moves its generation, so an edge-triggered entry reports what happened since it last
//! reported.

use alloc::{collections::BTreeMap, vec::Vec};

use crate::types::errno;

/// `eventfd2`'s flags.
pub mod efd {
    pub const SEMAPHORE: u64 = 1;
    pub const NONBLOCK: u64 = 0o4000;
    pub const CLOEXEC: u64 = 0o2000000;
    pub const ALL: u64 = SEMAPHORE | NONBLOCK | CLOEXEC;
}

/// `epoll`'s events, operations and flags.
pub mod ep {
    pub const IN: u32 = 0x001;
    pub const PRI: u32 = 0x002;
    pub const OUT: u32 = 0x004;
    pub const ERR: u32 = 0x008;
    pub const HUP: u32 = 0x010;
    pub const RDNORM: u32 = 0x040;
    pub const RDBAND: u32 = 0x080;
    pub const WRNORM: u32 = 0x100;
    pub const WRBAND: u32 = 0x200;
    pub const MSG: u32 = 0x400;
    pub const RDHUP: u32 = 0x2000;
    pub const EXCLUSIVE: u32 = 1 << 28;
    pub const WAKEUP: u32 = 1 << 29;
    pub const ONESHOT: u32 = 1 << 30;
    pub const ET: u32 = 1 << 31;
    /// The input and output events a descriptor can be ready for.
    pub const READY: u32 = IN | PRI | OUT | RDNORM | RDBAND | WRNORM | WRBAND | MSG | RDHUP;
    /// Every bit `epoll_ctl` takes.
    pub const ALL: u32 = READY | ERR | HUP | EXCLUSIVE | WAKEUP | ONESHOT | ET;
    pub const CTL_ADD: u64 = 1;
    pub const CTL_DEL: u64 = 2;
    pub const CTL_MOD: u64 = 3;
    /// `epoll_create1`'s one flag.
    pub const CLOEXEC: u64 = 0o2000000;
    /// The most events one wait returns, as Linux bounds `maxevents`.
    pub const MAX_EVENTS: u64 = (i32::MAX as u64) / 12;
    /// `struct epoll_event` on x86-64: packed, a `u32` and a `u64`.
    pub const EVENT_SIZE: usize = 12;
}

/// What an eventfd's read or write would do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step<T> {
    Done(T),
    /// It would block: `EAGAIN` for a descriptor opened `EFD_NONBLOCK`.
    WouldBlock,
}

/// An eventfd's counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventFd {
    count: u64,
    semaphore: bool,
    generation: u64,
}

impl EventFd {
    /// A counter starting at `initial`, with `eventfd2`'s flags (`EINVAL` for one it does
    /// not know).
    pub fn new(initial: u64, flags: u64) -> Result<EventFd, u64> {
        if flags & !efd::ALL != 0 || initial > u32::MAX as u64 {
            return Err(errno::EINVAL);
        }
        Ok(EventFd {
            count: initial,
            semaphore: flags & efd::SEMAPHORE != 0,
            generation: 0,
        })
    }

    /// The counter now.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Moves on every change, for edge-triggered entries.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// `read` of a buffer of `len` bytes: the counter (or 1), which it resets (or
    /// decrements); `EINVAL` under 8 bytes.
    pub fn read(&mut self, len: u64) -> Result<Step<u64>, u64> {
        if len < 8 {
            return Err(errno::EINVAL);
        }
        if self.count == 0 {
            return Ok(Step::WouldBlock);
        }
        let value = if self.semaphore { 1 } else { self.count };
        self.count -= value;
        self.generation += 1;
        Ok(Step::Done(value))
    }

    /// `write` of `value` from a buffer of `len` bytes; `EINVAL` for `u64::MAX` or under
    /// 8 bytes.
    pub fn write(&mut self, len: u64, value: u64) -> Result<Step<()>, u64> {
        if len < 8 || value == u64::MAX {
            return Err(errno::EINVAL);
        }
        if value > u64::MAX - 1 - self.count {
            return Ok(Step::WouldBlock);
        }
        if value > 0 {
            self.count += value;
            self.generation += 1;
        }
        Ok(Step::Done(()))
    }

    /// What it is ready for: input while nonzero, output while another 1 fits.
    pub fn ready(&self) -> u32 {
        let mut r = 0;
        if self.count > 0 {
            r |= ep::IN | ep::RDNORM;
        }
        if self.count < u64::MAX - 1 {
            r |= ep::OUT | ep::WRNORM;
        }
        r
    }
}

/// What a watched descriptor is now, as the server sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// Its open file description: an entry is for the description it was added with, as
    /// on Linux, so a number closed and opened again is another file.
    pub description: usize,
    /// The events it is ready for.
    pub ready: u32,
    /// Its generation (an eventfd's; 0 for a descriptor that never changes).
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Interest {
    description: usize,
    events: u32,
    data: u64,
    /// The generation an edge-triggered entry last reported; none before its first.
    reported: Option<u64>,
    /// A one-shot entry that has reported, until `EPOLL_CTL_MOD`.
    spent: bool,
}

/// An epoll instance: its interest list, by descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Epoll {
    interests: BTreeMap<u64, Interest>,
}

impl Epoll {
    pub fn new() -> Epoll {
        Epoll::default()
    }

    /// How many descriptors it watches.
    pub fn len(&self) -> usize {
        self.interests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.interests.is_empty()
    }

    /// `epoll_ctl` of descriptor `fd`, whose description is `description`, with the
    /// events and data of the caller's `struct epoll_event` (none for `EPOLL_CTL_DEL`).
    pub fn ctl(
        &mut self,
        op: u64,
        fd: u64,
        description: usize,
        event: Option<(u32, u64)>,
    ) -> Result<(), u64> {
        match op {
            ep::CTL_ADD => {
                let (events, data) = event.ok_or(errno::EFAULT)?;
                if events & !ep::ALL != 0 {
                    return Err(errno::EINVAL);
                }
                if self
                    .interests
                    .get(&fd)
                    .is_some_and(|i| i.description == description)
                {
                    return Err(errno::EEXIST);
                }
                self.interests.insert(
                    fd,
                    Interest {
                        description,
                        events,
                        data,
                        reported: None,
                        spent: false,
                    },
                );
                Ok(())
            }
            ep::CTL_MOD => {
                let (events, data) = event.ok_or(errno::EFAULT)?;
                if events & !ep::ALL != 0 || events & ep::EXCLUSIVE != 0 {
                    return Err(errno::EINVAL);
                }
                let entry = self
                    .interests
                    .get_mut(&fd)
                    .filter(|i| i.description == description)
                    .ok_or(errno::ENOENT)?;
                entry.events = events;
                entry.data = data;
                entry.reported = None;
                entry.spent = false;
                Ok(())
            }
            ep::CTL_DEL => match self.interests.get(&fd) {
                Some(i) if i.description == description => {
                    self.interests.remove(&fd);
                    Ok(())
                }
                _ => Err(errno::ENOENT),
            },
            _ => Err(errno::EINVAL),
        }
    }

    /// The events ready now, at most `max`, as `(events, data)`; edge-triggered and
    /// one-shot entries are marked reported. `seen` says what each watched descriptor is
    /// now, or `None` once it is closed, whose entry then goes, as Linux drops an entry
    /// when its file is closed.
    pub fn collect(
        &mut self,
        max: usize,
        mut seen: impl FnMut(u64) -> Option<Seen>,
    ) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (fd, interest) in self.interests.iter_mut() {
            let now = match seen(*fd) {
                Some(now) if now.description == interest.description => now,
                _ => {
                    gone.push(*fd);
                    continue;
                }
            };
            if out.len() >= max || interest.spent {
                continue;
            }
            let fired = now.ready & (interest.events | ep::ERR | ep::HUP);
            if fired == 0 {
                continue;
            }
            if interest.events & ep::ET != 0 && interest.reported == Some(now.generation) {
                continue;
            }
            out.push((fired, interest.data));
            if interest.events & ep::ET != 0 {
                interest.reported = Some(now.generation);
            }
            if interest.events & ep::ONESHOT != 0 {
                interest.spent = true;
            }
        }
        for fd in gone {
            self.interests.remove(&fd);
        }
        out
    }
}

/// `struct epoll_event`s as the caller's array holds them.
pub fn encode(events: &[(u32, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(events.len() * ep::EVENT_SIZE);
    for (bits, data) in events {
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&data.to_le_bytes());
    }
    out
}

/// One `struct epoll_event` read from the caller: its events and data.
pub fn decode(bytes: &[u8]) -> Option<(u32, u64)> {
    if bytes.len() < ep::EVENT_SIZE {
        return None;
    }
    let events = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let mut data = [0u8; 8];
    data.copy_from_slice(&bytes[4..12]);
    Some((events, u64::from_le_bytes(data)))
}

/// An `epoll_wait` timeout in milliseconds as a deadline on a clock now at `now`
/// nanoseconds: `Some(now)` for 0 (poll), `None` for a negative one (wait for ever).
pub fn deadline_ms(timeout: i32, now: u64) -> Option<u64> {
    if timeout < 0 {
        None
    } else {
        Some(now.saturating_add(timeout as u64 * 1_000_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(description: usize, e: &EventFd) -> Option<Seen> {
        Some(Seen {
            description,
            ready: e.ready(),
            generation: e.generation(),
        })
    }

    #[test]
    fn kd2_t1_an_eventfd_counts_as_linux_does() {
        let mut e = EventFd::new(0, efd::NONBLOCK | efd::CLOEXEC).unwrap();
        assert_eq!(e.read(8), Ok(Step::WouldBlock));
        assert_eq!(e.ready(), ep::OUT | ep::WRNORM);
        assert_eq!(e.write(8, 3), Ok(Step::Done(())));
        assert_eq!(e.write(8, 4), Ok(Step::Done(())));
        assert_eq!(e.ready(), ep::IN | ep::RDNORM | ep::OUT | ep::WRNORM);
        assert_eq!(e.read(8), Ok(Step::Done(7)), "a read takes the whole count");
        assert_eq!(e.count(), 0);
        assert_eq!(e.read(4), Err(errno::EINVAL));
        assert_eq!(e.write(4, 1), Err(errno::EINVAL));
        assert_eq!(e.write(8, u64::MAX), Err(errno::EINVAL));
        assert_eq!(e.write(8, u64::MAX - 1), Ok(Step::Done(())));
        assert_eq!(e.ready(), ep::IN | ep::RDNORM, "full: not writable");
        assert_eq!(e.write(8, 1), Ok(Step::WouldBlock));
        let g = e.generation();
        assert_eq!(e.write(8, 0), Ok(Step::Done(())));
        assert_eq!(e.generation(), g, "adding nothing changes nothing");
        let mut s = EventFd::new(2, efd::SEMAPHORE).unwrap();
        assert_eq!(s.read(8), Ok(Step::Done(1)));
        assert_eq!(s.read(8), Ok(Step::Done(1)));
        assert_eq!(s.read(8), Ok(Step::WouldBlock));
        assert_eq!(EventFd::new(0, 0o10), Err(errno::EINVAL));
        assert_eq!(EventFd::new(1 << 32, 0), Err(errno::EINVAL));
    }

    #[test]
    fn kd2_t1_epoll_reports_level_edge_and_one_shot() {
        let mut waker = EventFd::new(0, efd::NONBLOCK).unwrap();
        let mut level = EventFd::new(0, 0).unwrap();
        let mut once = EventFd::new(0, 0).unwrap();
        let mut ep = Epoll::new();
        ep.ctl(ep::CTL_ADD, 5, 50, Some((ep::IN | ep::ET, 0xAA)))
            .unwrap();
        ep.ctl(ep::CTL_ADD, 6, 60, Some((ep::IN, 0xBB))).unwrap();
        ep.ctl(ep::CTL_ADD, 7, 70, Some((ep::IN | ep::ONESHOT, 0xCC)))
            .unwrap();
        assert_eq!(ep.len(), 3);
        assert_eq!(
            ep.ctl(ep::CTL_ADD, 5, 50, Some((ep::IN, 0))),
            Err(errno::EEXIST)
        );
        let all = |ep: &mut Epoll, w: &EventFd, l: &EventFd, o: &EventFd| {
            ep.collect(16, |fd| match fd {
                5 => seen(50, w),
                6 => seen(60, l),
                7 => seen(70, o),
                _ => None,
            })
        };
        assert!(all(&mut ep, &waker, &level, &once).is_empty());
        waker.write(8, 1).unwrap();
        level.write(8, 1).unwrap();
        once.write(8, 1).unwrap();
        let got = all(&mut ep, &waker, &level, &once);
        // What fired is what was asked for (or an error or a hang-up), not all it is ready for.
        assert_eq!(got, [(ep::IN, 0xAA), (ep::IN, 0xBB), (ep::IN, 0xCC)]);
        // Unchanged: the level entry reports again, the edge and the one-shot do not.
        assert_eq!(all(&mut ep, &waker, &level, &once), [(ep::IN, 0xBB)]);
        // A second write is a new edge, though the counter was ready all along.
        waker.write(8, 1).unwrap();
        once.write(8, 1).unwrap();
        assert_eq!(
            all(&mut ep, &waker, &level, &once),
            [(ep::IN, 0xAA), (ep::IN, 0xBB)]
        );
        // MOD re-arms the one-shot.
        ep.ctl(ep::CTL_MOD, 7, 70, Some((ep::IN | ep::ONESHOT, 0xCD)))
            .unwrap();
        assert_eq!(
            all(&mut ep, &waker, &level, &once),
            [(ep::IN, 0xBB), (ep::IN, 0xCD)]
        );
        // `max` bounds what one wait returns; the rest stay for the next.
        assert_eq!(
            ep.collect(1, |fd| (fd == 6).then(|| seen(60, &level)).flatten())
                .len(),
            1
        );
        assert_eq!(ep.len(), 1, "closed descriptors' entries go");
    }

    #[test]
    fn kd2_t1_epoll_ctl_refusals_and_descriptions() {
        let e = EventFd::new(1, 0).unwrap();
        let mut ep = Epoll::new();
        assert_eq!(ep.ctl(ep::CTL_ADD, 3, 30, None), Err(errno::EFAULT));
        assert_eq!(
            ep.ctl(ep::CTL_ADD, 3, 30, Some((1 << 27, 0))),
            Err(errno::EINVAL)
        );
        assert_eq!(
            ep.ctl(ep::CTL_MOD, 3, 30, Some((ep::IN, 0))),
            Err(errno::ENOENT)
        );
        assert_eq!(ep.ctl(ep::CTL_DEL, 3, 30, None), Err(errno::ENOENT));
        assert_eq!(ep.ctl(9, 3, 30, None), Err(errno::EINVAL));
        ep.ctl(ep::CTL_ADD, 3, 30, Some((ep::IN | ep::OUT, 7)))
            .unwrap();
        assert_eq!(
            ep.ctl(ep::CTL_MOD, 3, 30, Some((ep::IN | ep::EXCLUSIVE, 0))),
            Err(errno::EINVAL)
        );
        // The number now names another description: the entry is not that file's.
        assert_eq!(ep.ctl(ep::CTL_DEL, 3, 31, None), Err(errno::ENOENT));
        assert!(ep.collect(8, |_| seen(31, &e)).is_empty());
        assert!(ep.is_empty(), "an entry whose file was closed goes");
        ep.ctl(ep::CTL_ADD, 3, 31, Some((ep::OUT, 1))).unwrap();
        assert_eq!(ep.collect(8, |_| seen(31, &e)), [(ep::OUT, 1)]);
        ep.ctl(ep::CTL_DEL, 3, 31, None).unwrap();
        assert!(ep.is_empty());
        // Error and hang-up are reported though not asked for.
        let mut hup = Epoll::new();
        hup.ctl(ep::CTL_ADD, 4, 40, Some((ep::IN, 2))).unwrap();
        let got = hup.collect(8, |_| {
            Some(Seen {
                description: 40,
                ready: ep::HUP,
                generation: 0,
            })
        });
        assert_eq!(got, [(ep::HUP, 2)]);
    }

    #[test]
    fn kd2_t1_events_encode_packed_and_timeouts() {
        let bytes = encode(&[(ep::IN, 0x1122_3344_5566_7788), (ep::OUT, 1)]);
        assert_eq!(bytes.len(), 2 * ep::EVENT_SIZE);
        assert_eq!(&bytes[..4], &ep::IN.to_le_bytes());
        assert_eq!(decode(&bytes[..12]), Some((ep::IN, 0x1122_3344_5566_7788)));
        assert_eq!(decode(&bytes[12..]), Some((ep::OUT, 1)));
        assert_eq!(decode(&bytes[..11]), None);
        assert_eq!(deadline_ms(-1, 5), None);
        assert_eq!(deadline_ms(0, 5), Some(5));
        assert_eq!(deadline_ms(3, 5), Some(3_000_005));
    }
}
