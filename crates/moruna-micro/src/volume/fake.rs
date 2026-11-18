//! A volume's driver in memory, for the tests of the volumes and of the reactor that reads and
//! writes them: requests answered by threads of its own, later than they are made, so an
//! operation's requests are seen on their way together.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use moruna_image::layerfile::{self, Data, Entry};

use super::io::{Bounce, Done, Driver, Geometry, Moved, Told};

/// Memory the fake driver reaches: a page-aligned piece of a vector.
pub(crate) struct Heap {
    buf: Vec<u8>,
    at: usize,
    len: usize,
}

impl Heap {
    pub(crate) fn new(len: usize) -> Heap {
        let buf = vec![0u8; len + 4096];
        let at = (4096 - buf.as_ptr() as usize % 4096) % 4096;
        Heap { buf, at, len }
    }
}

impl Bounce for Heap {
    fn addr(&self) -> u64 {
        self.buf[self.at..].as_ptr() as u64
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[self.at..self.at + self.len]
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.at..self.at + self.len]
    }
}

enum Job {
    Read(u64, u64, u32, Told),
    Write(u64, u64, u32, Told),
    Flush(Told),
}

/// A disk in memory, its requests answered by two threads after a short wait.
pub(crate) struct Fake {
    pub(crate) disk: Arc<Mutex<Vec<u8>>>,
    pub(crate) geometry: Geometry,
    /// Places a transfer only at a page's start, as a driver of protocol 2.0 through
    /// `read_at` does.
    page_only: bool,
    /// A request reaching this sector fails.
    fail: Option<u64>,
    jobs: Mutex<mpsc::Sender<Job>>,
    pub(crate) flushes: Arc<AtomicU64>,
    /// While closed, no request is answered.
    gate: Arc<(Mutex<bool>, Condvar)>,
}

fn copy_to(addr: u64, bytes: &[u8]) {
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
}

fn copy_from(addr: u64, out: &mut [u8]) {
    unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
}

impl Fake {
    pub(crate) fn new(bytes: Vec<u8>, sector: u64, max_sectors: u32, read_only: bool) -> Arc<Fake> {
        Fake::with(bytes, sector, max_sectors, read_only, false, None)
    }

    pub(crate) fn with(
        bytes: Vec<u8>,
        sector: u64,
        max_sectors: u32,
        read_only: bool,
        page_only: bool,
        fail: Option<u64>,
    ) -> Arc<Fake> {
        assert!((bytes.len() as u64).is_multiple_of(sector));
        let geometry = Geometry {
            sector,
            sectors: bytes.len() as u64 / sector,
            max_sectors,
            read_only,
        };
        let disk = Arc::new(Mutex::new(bytes));
        let flushes = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        for _ in 0..2 {
            let (rx, disk, flushes) = (Arc::clone(&rx), Arc::clone(&disk), Arc::clone(&flushes));
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                loop {
                    let job = match rx.lock().unwrap().recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    };
                    let (open, turn) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = turn.wait(open).unwrap();
                    }
                    drop(open);
                    match job {
                        Job::Read(addr, at, n, told) => {
                            let d = disk.lock().unwrap();
                            copy_to(addr, &d[at as usize..at as usize + n as usize]);
                            drop(d);
                            told(Moved::Done);
                        }
                        Job::Write(addr, at, n, told) => {
                            let mut d = disk.lock().unwrap();
                            copy_from(addr, &mut d[at as usize..at as usize + n as usize]);
                            drop(d);
                            told(Moved::Done);
                        }
                        Job::Flush(told) => {
                            flushes.fetch_add(1, Ordering::SeqCst);
                            told(Moved::Done);
                        }
                    }
                }
            });
        }
        Arc::new(Fake {
            disk,
            geometry,
            page_only,
            fail,
            jobs: Mutex::new(tx),
            flushes,
            gate,
        })
    }

    /// Holds every request until [`Fake::open`].
    pub(crate) fn close(&self) {
        *self.gate.0.lock().unwrap() = false;
    }

    pub(crate) fn open(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }

    /// The answer a request gets before it is queued, if it gets one.
    fn refuse(&self, addr: u64, sector: u64, count: u32) -> Option<Moved> {
        let g = self.geometry;
        assert!(
            count > 0 && count <= g.max_sectors,
            "a request of {count} sectors"
        );
        assert!(
            sector + u64::from(count) <= g.sectors,
            "a request past the end"
        );
        if self.page_only && !addr.is_multiple_of(4096) {
            return Some(Moved::Elsewhere);
        }
        if self
            .fail
            .is_some_and(|f| (sector..sector + u64::from(count)).contains(&f))
        {
            return Some(Moved::Failed("Io".into()));
        }
        None
    }
}

impl Driver for Fake {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn read_to(&self, addr: u64, sector: u64, count: u32, told: Told) {
        if let Some(answer) = self.refuse(addr, sector, count) {
            return told(answer);
        }
        let s = self.geometry.sector;
        let job = Job::Read(addr, sector * s, count * s as u32, told);
        self.jobs.lock().unwrap().send(job).unwrap();
    }

    fn write_from(&self, addr: u64, sector: u64, count: u32, told: Told) {
        if self.geometry.read_only {
            return told(Moved::Failed("ReadOnly".into()));
        }
        if let Some(answer) = self.refuse(addr, sector, count) {
            return told(answer);
        }
        let s = self.geometry.sector;
        let job = Job::Write(addr, sector * s, count * s as u32, told);
        self.jobs.lock().unwrap().send(job).unwrap();
    }

    fn flush(&self, told: Told) {
        self.jobs.lock().unwrap().send(Job::Flush(told)).unwrap();
    }

    fn bounce(&self, bytes: usize) -> Result<Box<dyn Bounce>, String> {
        Ok(Box::new(Heap::new(bytes)))
    }
}

/// Runs a read or a write and waits for it.
pub(crate) fn wait(op: impl FnOnce(Done)) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    op(Box::new(move |r| tx.send(r).unwrap()));
    rx.recv_timeout(Duration::from_secs(30)).unwrap()
}

/// Bytes no two runs of which repeat soon.
pub(crate) fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

pub(crate) fn file(bytes: Vec<u8>) -> Entry {
    Entry::File {
        mode: 0o644,
        data: Data::Bytes(bytes.into()),
        link: None,
    }
}

pub(crate) fn dir(children: Vec<(&str, Entry)>) -> Entry {
    Entry::Directory {
        mode: 0o755,
        opaque: false,
        children: children
            .into_iter()
            .map(|(n, e)| (n.as_bytes().to_vec(), e))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// The bytes of a layer holding `root`, as `moruna volume pack` writes one, on a volume of
/// `extra` more bytes.
pub(crate) fn layer(root: &Entry, extra: usize) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!(
        "moruna-volume-{}-{}",
        std::process::id(),
        pattern(
            8,
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos() as u64
        )
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("layer.img");
    layerfile::write(root, &out).unwrap();
    let mut bytes = std::fs::read(&out).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    bytes.resize(bytes.len() + extra, 0);
    bytes
}
