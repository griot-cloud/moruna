//! Host tests of the `storvsc` process (work item 1.10b): its scan, its names and its block
//! service, against `moruna-storvsc`'s fake storvsp behind `moruna-vmbus`'s in-memory
//! channel, with clients that reach it through `moruna.driver.block` on the SDK's model of
//! channels.

#[allow(dead_code)]
#[path = "../../../core/tests/storvsc/fake.rs"]
mod fake;

mod scan;
mod serve;

use std::{cell::RefCell, collections::BTreeMap, fmt, rc::Rc};

use fake::{Host, Link};
use moruna_storvsc::{Address, PAGE, Storvsc};
use moruna_storvsc_server::{Controller, Node, Plan, plan, scan::Scratch};
use moruna_svc::{Memory, Rights, map};
use vmbus_protocol::Guid;

/// Guest pages the fake has: the scan's page, then the pins'.
const GUEST_PAGES: usize = 4096;

/// The scan's page and clock, over the fake's guest memory: page 0, and a clock that stands.
struct Page(Rc<RefCell<Host>>);

impl Scratch for Page {
    fn page(&self) -> u64 {
        0
    }

    fn read(&self, out: &mut [u8]) {
        let bytes = self.0.borrow().memory.read(&[0], out.len()).unwrap();
        out.copy_from_slice(&bytes);
    }

    fn now(&self) -> u64 {
        0
    }
}

/// A client's memory pinned for the fake host: guest pages of the fake's memory, which the
/// node copies the memory into and out of, as a host that reaches guest memory through
/// copies would.
pub struct Pin {
    memory: Memory,
    pages: Vec<u64>,
    device_writes: bool,
}

/// The node under the driver, over the fake host of its one controller.
pub struct FakeNode {
    host: Rc<RefCell<Host>>,
    next_page: u64,
    pub releases: usize,
    pub console: Vec<String>,
}

impl FakeNode {
    fn new(host: Rc<RefCell<Host>>) -> FakeNode {
        FakeNode {
            host,
            next_page: 1,
            releases: 0,
            console: Vec::new(),
        }
    }

    pub fn said(&self, text: &str) -> bool {
        self.console.iter().any(|line| line.contains(text))
    }
}

/// The pages of a memory object of the SDK's host model, which says only whether a mapping
/// fits.
fn pages_of(memory: &Memory) -> usize {
    (1..=256)
        .take_while(|n| memory.map(0, *n as u64 * PAGE as u64, 0).is_ok())
        .count()
}

impl Node for FakeNode {
    type Pin = Pin;

    fn pin(&mut self, memory: &Memory, device_writes: bool) -> Option<(Pin, Vec<u64>)> {
        let (_, rights) = memory.0.info().ok()?;
        let needed = if device_writes {
            Rights::READ | Rights::WRITE | Rights::MAP | Rights::DUPLICATE
        } else {
            Rights::READ | Rights::MAP | Rights::DUPLICATE
        };
        if !rights.contains(needed) {
            return None;
        }
        let n = pages_of(memory);
        let pages: Vec<u64> = (self.next_page..self.next_page + n as u64).collect();
        self.next_page += n as u64;
        assert!(self.next_page as usize <= GUEST_PAGES, "the fake's pages");
        let memory = Memory(memory.0.duplicate(needed).ok()?);
        Some((
            Pin {
                memory,
                pages: pages.clone(),
                device_writes,
            },
            pages,
        ))
    }

    fn sync(&mut self, pin: &Pin, to_device: bool) {
        let len = pin.pages.len() * PAGE;
        let mut bytes = vec![0; len];
        if to_device {
            let mapping = pin.memory.map(0, len as u64, 0).unwrap();
            mapping.read(0, &mut bytes).unwrap();
            self.host
                .borrow_mut()
                .memory
                .write(&pin.pages, &bytes)
                .unwrap();
        } else {
            assert!(
                pin.device_writes,
                "the host wrote memory pinned for reading"
            );
            let from = self.host.borrow().memory.read(&pin.pages, len).unwrap();
            let mapping = pin.memory.map(0, len as u64, map::WRITE).unwrap();
            mapping.write(0, &from).unwrap();
        }
    }

    fn release(&mut self) {
        self.releases += 1;
    }

    fn say(&mut self, line: fmt::Arguments<'_>) {
        self.console.push(line.to_string());
    }
}

/// A controller of the fake storvsp with these disks, `(lun, block size, blocks)`, at
/// target 0, and its host.
fn controller(disks: &[(u8, u32, u64)]) -> (Link, Rc<RefCell<Host>>) {
    let (link, host) = fake::pair();
    {
        let mut host = host.borrow_mut();
        host.memory = fake::Memory::new(GUEST_PAGES);
        host.disks = disks
            .iter()
            .map(|(lun, size, blocks)| ((0, 0, *lun), fake::FakeDisk::new(*size, *blocks, b"")))
            .collect::<BTreeMap<_, _>>();
    }
    (link, host)
}

/// The controller started and scanned.
fn scanned(
    link: Link,
    host: &Rc<RefCell<Host>>,
) -> (Storvsc<Link>, moruna_storvsc_server::scan::Scan) {
    let page = Page(host.clone());
    let mut device = moruna_storvsc_server::scan::start(link, &page).unwrap();
    let scan = moruna_storvsc_server::scan::scan(&mut device, &page).unwrap();
    (device, scan)
}

/// The plan for one controller of instance `instance`.
fn plan_of(instance: Guid, scan: &moruna_storvsc_server::scan::Scan) -> Plan {
    plan(&[Controller { instance, scan }])
}

/// The address of `lun` at the scanned target.
fn at(lun: u8) -> Address {
    Address {
        path: 0,
        target: 0,
        lun,
    }
}

/// Writes a GPT onto `bytes`, a disk of 512-byte blocks: the primary header at block 1, 128
/// entries of 128 bytes from block 2, the partitions `parts` (`name`, first block, last).
pub fn gpt(bytes: &mut [u8], parts: &[(&str, u64, u64)]) {
    let blocks = bytes.len() as u64 / 512;
    let mut table = vec![0u8; 128 * 128];
    for (i, (name, first, last)) in parts.iter().enumerate() {
        let e = &mut table[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&[0xaf; 16]);
        e[16..32].copy_from_slice(&[i as u8 + 1; 16]);
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (j, unit) in name.encode_utf16().enumerate() {
            e[56 + 2 * j..58 + 2 * j].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let mut h = vec![0u8; 512];
    h[..8].copy_from_slice(moruna_disk::SIGNATURE);
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&1u64.to_le_bytes());
    h[32..40].copy_from_slice(&(blocks - 1).to_le_bytes());
    h[40..48].copy_from_slice(&34u64.to_le_bytes());
    h[48..56].copy_from_slice(&(blocks - 34).to_le_bytes());
    h[56..72].copy_from_slice(&[0x42; 16]);
    h[72..80].copy_from_slice(&2u64.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&moruna_disk::crc32(&table).to_le_bytes());
    let crc = moruna_disk::crc32(&h[..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    bytes[512..1024].copy_from_slice(&h);
    bytes[1024..1024 + table.len()].copy_from_slice(&table);
}
