//! One volume's bytes moved through its driver: a read of any byte range lands its whole
//! sectors in place and passes only the edge sectors through a bounce; a write is whole
//! sectors from memory as it is; every request is at most the driver's largest, and every
//! request of an operation is on its way at once.
//!
//! A request the driver cannot place where it was asked (it answers `BadBuffer`, or predates
//! `moruna.driver.block` 2.2) moves through a bounce instead: that request alone, counted in
//! [`Stats::fallbacks`], and said once on the log for the volume, never silently for the run.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// What a driver says of its volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes in a sector: 512 or a larger power of two.
    pub sector: u64,
    /// The volume's capacity, in sectors.
    pub sectors: u64,
    /// The most sectors one request moves.
    pub max_sectors: u32,
    /// The driver refuses every write.
    pub read_only: bool,
}

/// How a request to the driver ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Moved {
    /// The sectors moved.
    Done,
    /// The driver cannot place them at that address: nothing moved, and the request may be
    /// made again through a bounce.
    Elsewhere,
    /// The driver or its device failed the request.
    Failed(String),
}

/// Told once, on any thread, when a request to the driver ends.
pub type Told = Box<dyn FnOnce(Moved) + Send>;

/// Told once, on any thread, when an operation ends.
pub type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

/// Memory the driver reaches, which an edge or a request it cannot place passes through.
pub trait Bounce: Send {
    /// Its first byte's address in this process.
    fn addr(&self) -> u64;
    /// Its bytes.
    fn bytes(&self) -> &[u8];
    /// Its bytes, to write.
    fn bytes_mut(&mut self) -> &mut [u8];
}

/// A volume's driver, as the engine reaches it (`moruna.driver.block`): whole sectors moved
/// between the volume and this process's memory, many requests on their way at once.
pub trait Driver: Send + Sync {
    /// The volume's sectors.
    fn geometry(&self) -> Geometry;
    /// Reads `count` sectors from `sector` into the bytes at `addr`, which the caller keeps
    /// alive until `told`.
    fn read_to(&self, addr: u64, sector: u64, count: u32, told: Told);
    /// Writes `count` sectors at `sector` from the bytes at `addr`.
    fn write_from(&self, addr: u64, sector: u64, count: u32, told: Told);
    /// Puts every write answered so far on stable storage.
    fn flush(&self, told: Told);
    /// At least `bytes` of memory it reaches, for a bounce.
    fn bounce(&self, bytes: usize) -> Result<Box<dyn Bounce>, String>;
}

/// What a volume has moved, for the run's report and the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Requests made to the driver.
    pub requests: u64,
    /// Bytes moved straight between the volume and the operations' own memory.
    pub placed_bytes: u64,
    /// Bytes copied through a bounce: the edges, and the requests the driver could not place.
    pub bounced_bytes: u64,
    /// Requests the driver could not place, made again through a bounce.
    pub fallbacks: u64,
    /// The most requests on their way at once.
    pub in_flight_max: u64,
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    placed_bytes: AtomicU64,
    bounced_bytes: AtomicU64,
    fallbacks: AtomicU64,
    in_flight: AtomicU64,
    in_flight_max: AtomicU64,
    /// The fallback has been said on the log.
    said: AtomicBool,
}

/// One volume, by the serial the cloud gives it, and how the job may use it.
#[derive(Clone)]
pub struct Volume {
    serial: String,
    driver: Arc<dyn Driver>,
    geometry: Geometry,
    writable: bool,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("serial", &self.serial)
            .field("geometry", &self.geometry)
            .field("writable", &self.writable)
            .finish()
    }
}

/// What an operation's requests have come to so far.
struct Joined {
    left: usize,
    failed: Option<String>,
    done: Option<Done>,
}

/// An operation's requests: told when the last ends, with the first failure.
#[derive(Clone)]
pub(crate) struct Join(Arc<Mutex<Joined>>);

impl Join {
    /// An operation of `parts` requests, `done` told when the last ends.
    pub(crate) fn new(parts: usize, done: Done) -> Join {
        Join(Arc::new(Mutex::new(Joined {
            left: parts,
            failed: None,
            done: Some(done),
        })))
    }

    /// One request ended.
    pub(crate) fn part(&self, result: Result<(), String>) {
        let mut j = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(why) = result {
            j.failed.get_or_insert(why);
        }
        j.left -= 1;
        if j.left == 0
            && let Some(done) = j.done.take()
        {
            let why = j.failed.take();
            drop(j);
            done(why.map_or(Ok(()), Err));
        }
    }
}

/// Copies `bytes` to `addr` of an operation's memory.
#[allow(unsafe_code)]
fn copy_in(addr: u64, bytes: &[u8]) {
    // SAFETY: `addr .. addr + bytes.len()` lies inside an operation's memory (each caller
    // computes it inside `[addr, addr + len)` of the operation), which its caller keeps alive
    // and touches nowhere else until the operation's `done` is told; the driver has finished
    // the request whose bytes these are.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
}

/// Copies the `out.len()` bytes at `addr` of an operation's memory into `out`.
#[allow(unsafe_code)]
fn copy_out(addr: u64, out: &mut [u8]) {
    // SAFETY: as `copy_in`: inside an operation's memory, alive and unchanged until `done`.
    unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
}

/// Fills `len` bytes at `addr` of an operation's memory with zeros.
pub(crate) fn zero(addr: u64, len: usize) {
    const ZEROS: [u8; 4096] = [0; 4096];
    let mut at = 0;
    while at < len {
        let n = (len - at).min(ZEROS.len());
        copy_in(addr + at as u64, &ZEROS[..n]);
        at += n;
    }
}

/// One request an operation makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    /// Whole sectors from `sector`, into or out of the operation's memory at byte `at`.
    Placed { sector: u64, count: u32, at: u64 },
    /// One sector through a bounce, of which `len` bytes from `skip` go to byte `at`.
    Edge {
        sector: u64,
        skip: usize,
        len: usize,
        at: u64,
    },
}

/// The requests that read `len` bytes at byte `offset` of a volume of `sector`-byte sectors,
/// no placed request longer than `most` sectors.
fn plan_read(offset: u64, len: u64, sector: u64, most: u32) -> Vec<Part> {
    let mut parts = Vec::new();
    if len == 0 {
        return parts;
    }
    let end = offset + len;
    let first = offset / sector;
    let last = (end - 1) / sector;
    let head = !offset.is_multiple_of(sector);
    let tail = !end.is_multiple_of(sector);
    if first == last && (head || tail) {
        parts.push(Part::Edge {
            sector: first,
            skip: (offset % sector) as usize,
            len: len as usize,
            at: 0,
        });
        return parts;
    }
    if head {
        parts.push(Part::Edge {
            sector: first,
            skip: (offset % sector) as usize,
            len: (sector - offset % sector) as usize,
            at: 0,
        });
    }
    let body = offset.div_ceil(sector);
    let body_end = end / sector;
    let mut s = body;
    while s < body_end {
        let count = (body_end - s).min(u64::from(most)) as u32;
        parts.push(Part::Placed {
            sector: s,
            count,
            at: s * sector - offset,
        });
        s += u64::from(count);
    }
    if tail {
        parts.push(Part::Edge {
            sector: last,
            skip: 0,
            len: (end % sector) as usize,
            at: last * sector - offset,
        });
    }
    parts
}

impl Volume {
    /// The volume `serial` its driver serves, for reading, or for writing too when
    /// `writable`: refused when the driver says it refuses every write.
    pub fn new(serial: &str, driver: Arc<dyn Driver>, writable: bool) -> Result<Volume, String> {
        let geometry = driver.geometry();
        if geometry.sector < 512 || !geometry.sector.is_power_of_two() || geometry.max_sectors == 0
        {
            return Err(format!(
                "volume {serial}: its driver says sectors of {} bytes, {} a request",
                geometry.sector, geometry.max_sectors
            ));
        }
        if writable && geometry.read_only {
            return Err(format!(
                "volume {serial} is a sink's, and its driver refuses every write"
            ));
        }
        Ok(Volume {
            serial: serial.to_string(),
            driver,
            geometry,
            writable,
            counters: Arc::default(),
        })
    }

    /// The serial it is named by.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Its sectors.
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Its bytes.
    pub fn capacity(&self) -> u64 {
        self.geometry.sectors * self.geometry.sector
    }

    /// Whether the job may write it.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// What it has moved so far.
    pub fn stats(&self) -> Stats {
        let c = &self.counters;
        Stats {
            requests: c.requests.load(Ordering::Relaxed),
            placed_bytes: c.placed_bytes.load(Ordering::Relaxed),
            bounced_bytes: c.bounced_bytes.load(Ordering::Relaxed),
            fallbacks: c.fallbacks.load(Ordering::Relaxed),
            in_flight_max: c.in_flight_max.load(Ordering::Relaxed),
        }
    }

    fn check(&self, what: &str, offset: u64, len: u64) -> Result<(), String> {
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.capacity())
        {
            return Err(format!(
                "{what} of {len} bytes at {offset} is past volume {}'s {} bytes",
                self.serial,
                self.capacity()
            ));
        }
        Ok(())
    }

    /// A request on its way: counted, and uncounted when `told`.
    fn told(&self, then: impl FnOnce(Moved) + Send + 'static) -> Told {
        let c = &self.counters;
        c.requests.fetch_add(1, Ordering::Relaxed);
        let now = c.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        c.in_flight_max.fetch_max(now, Ordering::Relaxed);
        let counters = Arc::clone(&self.counters);
        Box::new(move |moved| {
            counters.in_flight.fetch_sub(1, Ordering::SeqCst);
            then(moved)
        })
    }

    fn fell_back(&self) {
        self.counters.fallbacks.fetch_add(1, Ordering::Relaxed);
        if !self.counters.said.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                volume = %self.serial,
                "the volume's driver cannot place a transfer where it was asked; such a \
                 transfer passes through a bounce, one request at a time"
            );
        }
    }

    /// Reads `len` bytes at byte `offset` into the bytes at `addr`, which the caller keeps
    /// alive and untouched until `done`.
    pub fn read(&self, offset: u64, addr: u64, len: u64, done: Done) {
        if let Err(why) = self.check("a read", offset, len) {
            return done(Err(why));
        }
        let g = self.geometry;
        let parts = plan_read(offset, len, g.sector, g.max_sectors);
        if parts.is_empty() {
            return done(Ok(()));
        }
        let join = Join::new(parts.len(), done);
        for part in parts {
            match part {
                Part::Placed { sector, count, at } => {
                    self.placed_read(addr + at, sector, count, join.clone())
                }
                Part::Edge {
                    sector,
                    skip,
                    len,
                    at,
                } => self.bounced_read(sector, 1, skip, len, addr + at, join.clone()),
            }
        }
    }

    fn placed_read(&self, addr: u64, sector: u64, count: u32, join: Join) {
        let me = self.clone();
        let bytes = u64::from(count) * self.geometry.sector;
        self.driver.read_to(
            addr,
            sector,
            count,
            self.told(move |moved| match moved {
                Moved::Done => {
                    me.counters.placed_bytes.fetch_add(bytes, Ordering::Relaxed);
                    join.part(Ok(()));
                }
                Moved::Elsewhere => {
                    me.fell_back();
                    me.bounced_read(sector, count, 0, bytes as usize, addr, join);
                }
                Moved::Failed(why) => join.part(Err(me.failed("reading", sector, &why))),
            }),
        );
    }

    /// Reads `count` sectors from `sector` into a bounce, then `len` of its bytes from `skip`
    /// to `addr`.
    fn bounced_read(
        &self,
        sector: u64,
        count: u32,
        skip: usize,
        len: usize,
        addr: u64,
        join: Join,
    ) {
        let bytes = count as usize * self.geometry.sector as usize;
        let bounce = match self.driver.bounce(bytes) {
            Ok(b) => b,
            Err(why) => return join.part(Err(format!("volume {}: {why}", self.serial))),
        };
        let at = bounce.addr();
        let me = self.clone();
        self.driver.read_to(
            at,
            sector,
            count,
            self.told(move |moved| match moved {
                Moved::Done => {
                    copy_in(addr, &bounce.bytes()[skip..skip + len]);
                    me.counters
                        .bounced_bytes
                        .fetch_add(len as u64, Ordering::Relaxed);
                    join.part(Ok(()));
                }
                Moved::Elsewhere => join.part(Err(format!(
                    "volume {}: its driver refused its own bounce",
                    me.serial
                ))),
                Moved::Failed(why) => join.part(Err(me.failed("reading", sector, &why))),
            }),
        );
    }

    fn failed(&self, what: &str, sector: u64, why: &str) -> String {
        format!("volume {}: {what} at sector {sector}: {why}", self.serial)
    }

    /// Writes the `len` bytes at `addr`, whole sectors, at byte `offset`: refused unless the
    /// job may write the volume.
    pub fn write(&self, offset: u64, addr: u64, len: u64, done: Done) {
        if !self.writable {
            return done(Err(format!(
                "volume {} is a source's, which is read only",
                self.serial
            )));
        }
        if let Err(why) = self.check("a write", offset, len) {
            return done(Err(why));
        }
        let s = self.geometry.sector;
        if !offset.is_multiple_of(s) || !len.is_multiple_of(s) {
            return done(Err(format!(
                "volume {}: a write of {len} bytes at {offset} is not whole sectors of {s}",
                self.serial
            )));
        }
        let most = u64::from(self.geometry.max_sectors) * s;
        let mut parts = Vec::new();
        let mut at = 0;
        while at < len {
            let n = (len - at).min(most);
            parts.push((at, n));
            at += n;
        }
        if parts.is_empty() {
            return done(Ok(()));
        }
        let join = Join::new(parts.len(), done);
        for (at, n) in parts {
            self.placed_write(addr + at, (offset + at) / s, (n / s) as u32, join.clone());
        }
    }

    fn placed_write(&self, addr: u64, sector: u64, count: u32, join: Join) {
        let me = self.clone();
        let bytes = u64::from(count) * self.geometry.sector;
        self.driver.write_from(
            addr,
            sector,
            count,
            self.told(move |moved| match moved {
                Moved::Done => {
                    me.counters.placed_bytes.fetch_add(bytes, Ordering::Relaxed);
                    join.part(Ok(()));
                }
                Moved::Elsewhere => {
                    me.fell_back();
                    me.bounced_write(addr, sector, count, join);
                }
                Moved::Failed(why) => join.part(Err(me.failed("writing", sector, &why))),
            }),
        );
    }

    fn bounced_write(&self, addr: u64, sector: u64, count: u32, join: Join) {
        let bytes = count as usize * self.geometry.sector as usize;
        let mut bounce = match self.driver.bounce(bytes) {
            Ok(b) => b,
            Err(why) => return join.part(Err(format!("volume {}: {why}", self.serial))),
        };
        copy_out(addr, &mut bounce.bytes_mut()[..bytes]);
        let at = bounce.addr();
        let me = self.clone();
        self.driver.write_from(
            at,
            sector,
            count,
            self.told(move |moved| {
                drop(bounce);
                match moved {
                    Moved::Done => {
                        me.counters
                            .bounced_bytes
                            .fetch_add(bytes as u64, Ordering::Relaxed);
                        join.part(Ok(()));
                    }
                    Moved::Elsewhere => join.part(Err(format!(
                        "volume {}: its driver refused its own bounce",
                        me.serial
                    ))),
                    Moved::Failed(why) => join.part(Err(me.failed("writing", sector, &why))),
                }
            }),
        );
    }

    /// Puts every write done so far on stable storage.
    pub fn flush(&self, done: Done) {
        if !self.writable {
            return done(Ok(()));
        }
        let serial = self.serial.clone();
        self.driver.flush(self.told(move |moved| {
            done(match moved {
                Moved::Done => Ok(()),
                Moved::Elsewhere | Moved::Failed(_) => {
                    Err(format!("volume {serial}: the flush failed: {moved:?}"))
                }
            })
        }));
    }

    /// Runs one operation and waits for it, for the reads made outside the reactor (a
    /// layer's index): `op` is given the `done` to tell.
    pub fn wait(&self, op: impl FnOnce(Done)) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        op(Box::new(move |r| {
            let _ = tx.send(r);
        }));
        rx.recv()
            .unwrap_or_else(|_| Err(format!("volume {}: an operation was dropped", self.serial)))
    }

    /// The `len` bytes at byte `offset`, read through a bounce, one request at a time: for the
    /// few reads made outside the reactor (a layer's index), into memory no driver reaches.
    pub fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        self.check("a read", offset, len as u64)?;
        let s = self.geometry.sector;
        let most = u64::from(self.geometry.max_sectors) * s;
        let mut out = Vec::with_capacity(len);
        let end = offset + len as u64;
        let mut at = offset / s * s;
        while at < end {
            let n = (end.next_multiple_of(s) - at).min(most);
            let bounce = self
                .driver
                .bounce(n as usize)
                .map_err(|why| format!("volume {}: {why}", self.serial))?;
            let (tx, rx) = mpsc::channel();
            self.driver.read_to(
                bounce.addr(),
                at / s,
                (n / s) as u32,
                self.told(move |moved| {
                    let _ = tx.send(moved);
                }),
            );
            match rx.recv() {
                Ok(Moved::Done) => {}
                Ok(moved) => {
                    return Err(format!(
                        "volume {}: reading {n} bytes at {at}: {moved:?}",
                        self.serial
                    ));
                }
                Err(_) => return Err(format!("volume {}: a read was dropped", self.serial)),
            }
            let from = offset.max(at) - at;
            let to = (end - at).min(n);
            out.extend_from_slice(&bounce.bytes()[from as usize..to as usize]);
            self.counters
                .bounced_bytes
                .fetch_add(to - from, Ordering::Relaxed);
            at += n;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_is_its_edges_and_its_whole_sectors_in_requests_of_at_most_the_largest() {
        // Inside one sector.
        assert_eq!(
            plan_read(700, 100, 512, 8),
            [Part::Edge {
                sector: 1,
                skip: 188,
                len: 100,
                at: 0
            }]
        );
        // Whole sectors only, cut at the largest request.
        assert_eq!(
            plan_read(1024, 512 * 10, 512, 4),
            [
                Part::Placed {
                    sector: 2,
                    count: 4,
                    at: 0
                },
                Part::Placed {
                    sector: 6,
                    count: 4,
                    at: 2048
                },
                Part::Placed {
                    sector: 10,
                    count: 2,
                    at: 4096
                },
            ]
        );
        // Both edges and a body, on 4 KiB sectors.
        assert_eq!(
            plan_read(100, 3 * 4096, 4096, 256),
            [
                Part::Edge {
                    sector: 0,
                    skip: 100,
                    len: 3996,
                    at: 0
                },
                Part::Placed {
                    sector: 1,
                    count: 2,
                    at: 3996
                },
                Part::Edge {
                    sector: 3,
                    skip: 0,
                    len: 100,
                    at: 3996 + 8192
                },
            ]
        );
        // Two edges in neighbouring sectors, no body.
        assert_eq!(
            plan_read(500, 100, 512, 8),
            [
                Part::Edge {
                    sector: 0,
                    skip: 500,
                    len: 12,
                    at: 0
                },
                Part::Edge {
                    sector: 1,
                    skip: 0,
                    len: 88,
                    at: 12
                },
            ]
        );
        assert!(plan_read(5, 0, 512, 8).is_empty());
    }
}
