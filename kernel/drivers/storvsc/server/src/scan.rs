//! Finding the disks, once, at start: each controller's initialisation, then the LUNs of its
//! target with REPORT LUNS, as Linux's scan does on Hyper-V, and READ CAPACITY 16 of each.
//! A controller that refuses REPORT LUNS has LUNs 0 to 63, Azure's range, probed with READ
//! CAPACITY instead (the PM's decision for 1.10b). Disks attached while the node runs are
//! not found: a known limit.
//!
//! Every step waits with a deadline, and one that passes ends the scan with a reason.
//!
//! The boot disk's GPT is read too ([`boot_table`]): its disk GUID, which the image's must be,
//! and its `moruna-system` partition, which holds the node's engine layer and is served as a
//! window of it (item 3.3).

use alloc::{format, string::String, vec, vec::Vec};

use moruna_storvsc::{
    Address, Channel, Completion, Disk, Error, Event, Failure, SrbStatus, Storvsc, Wait, luns,
    scsi::LUNS_BYTES,
};

/// How long each step of the scan may take: the initialisation, and each command.
pub const STEP_NS: u64 = 10_000_000_000;

/// The target scanned on each controller: Hyper-V puts every disk at path 0, target 0, its
/// LUN its place on the controller.
pub const TARGET: Address = Address {
    path: 0,
    target: 0,
    lun: 0,
};

/// The LUNs probed when a controller refuses REPORT LUNS: Azure attaches data disks at LUNs
/// 0 to 63.
pub const PROBED: core::ops::RangeInclusive<u8> = 0..=63;

/// The page the host writes the scan's answers into, and the clock its deadlines are on.
pub trait Scratch {
    /// The page's guest page number.
    fn page(&self) -> u64;

    /// The page's first bytes, as the host wrote them.
    fn read(&self, out: &mut [u8]);

    /// Now, in nanoseconds on the node's monotonic clock.
    fn now(&self) -> u64;
}

/// What the scan of one controller found, and what it says of what it did not serve.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The disks, in LUN order.
    pub disks: Vec<Disk>,
    /// One line each for a LUN that answered and is no disk, and for a refused REPORT LUNS.
    pub notes: Vec<String>,
}

/// Whether a request block's status says there is no device at the address, rather than a
/// device that failed the command.
pub fn no_device(srb: SrbStatus) -> bool {
    matches!(
        srb,
        SrbStatus::INVALID_LUN
            | SrbStatus::INVALID_TARGET_ID
            | SrbStatus::INVALID_PATH_ID
            | SrbStatus::NO_DEVICE
            | SrbStatus::NO_HBA
            | SrbStatus::SELECTION_TIMEOUT
    )
}

/// The device on `channel`, once its initialisation has finished, by the deadline.
pub fn start<C: Channel, S: Scratch>(channel: C, scratch: &S) -> Result<Storvsc<C>, String> {
    let mut device = Storvsc::new(channel).map_err(|e| format!("the initialisation: {e}"))?;
    let deadline = scratch.now().saturating_add(STEP_NS);
    loop {
        match device.poll() {
            Ok(Some(Event::Ready)) => return Ok(device),
            Ok(_) => wait(&mut device, deadline, "the initialisation")?,
            Err(e) => return Err(format!("the initialisation: {e}")),
        }
    }
}

fn wait<C: Channel>(device: &mut Storvsc<C>, deadline: u64, what: &str) -> Result<(), String> {
    match device.wait(Some(deadline)) {
        Ok(Wait::Signalled) => Ok(()),
        Ok(Wait::TimedOut) => Err(format!("{what} was not answered within 10 s")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

/// Sends a command with `send` and waits for its end: the bytes it moved, or why not.
fn call<C: Channel, S: Scratch>(
    device: &mut Storvsc<C>,
    scratch: &S,
    what: &str,
    send: impl FnOnce(&mut Storvsc<C>) -> Result<u64, Error>,
) -> Result<Result<u32, Failure>, String> {
    let id = send(device).map_err(|e| format!("{what}: {e}"))?;
    let deadline = scratch.now().saturating_add(STEP_NS);
    loop {
        match device.poll() {
            Ok(Some(Event::Completed(Completion { id: done, result }))) if done == id => {
                return Ok(result);
            }
            // Nothing else is in flight: another completion is the host's error, and
            // the library has refused it.
            Ok(Some(_)) => {}
            Ok(None) => wait(device, deadline, what)?,
            Err(e) => return Err(format!("{what}: {e}")),
        }
    }
}

/// The bytes the host wrote into the scratch page.
fn answer<S: Scratch>(scratch: &S, moved: u32) -> Vec<u8> {
    let mut bytes = vec![0; moved as usize];
    scratch.read(&mut bytes);
    bytes
}

/// The disks of `device`'s controller.
pub fn scan<C: Channel, S: Scratch>(device: &mut Storvsc<C>, scratch: &S) -> Result<Scan, String> {
    let mut found = Scan::default();
    let page = scratch.page();
    let listed = call(device, scratch, "REPORT LUNS", |d| {
        d.report_luns(TARGET, page)
    })?;
    let (luns, probing) = match listed {
        Ok(moved) => match luns(&answer(scratch, moved.min(LUNS_BYTES))) {
            Ok(luns) => (luns, false),
            Err(malformed) => {
                found.notes.push(format!(
                    "REPORT LUNS answered {malformed}; LUNs 0 to 63 probed"
                ));
                (PROBED.collect(), true)
            }
        },
        Err(Failure::Scsi(error)) if no_device(error.srb) => (Vec::new(), false),
        Err(failure) => {
            found.notes.push(format!(
                "REPORT LUNS was refused ({failure:?}); LUNs 0 to 63 probed"
            ));
            (PROBED.collect(), true)
        }
    };
    for lun in luns {
        let address = Address { lun, ..TARGET };
        let what = format!("READ CAPACITY of LUN {lun}");
        match call(device, scratch, &what, |d| d.read_capacity(address, page))? {
            Ok(moved) => match Disk::from_capacity(address, &answer(scratch, moved)) {
                Ok(disk) => found.disks.push(disk),
                Err(malformed) => found.notes.push(format!(
                    "lun {lun}: its capacity is {malformed}; not served"
                )),
            },
            // A LUN only probed for is no disk when nothing is there.
            Err(Failure::Scsi(error)) if probing && no_device(error.srb) => {}
            Err(failure) => found
                .notes
                .push(format!("lun {lun}: no capacity ({failure:?}); not served")),
        }
    }
    Ok(found)
}

/// The bytes of a page.
const PAGE_BYTES: u64 = 4096;

/// Reads `count` blocks from `block` of `disk` into the scratch page: its bytes, or `None` when
/// the host fails the read.
fn read_page<C: Channel, S: Scratch>(
    device: &mut Storvsc<C>,
    scratch: &S,
    disk: &Disk,
    block: u64,
    count: u32,
) -> Result<Option<Vec<u8>>, String> {
    let page = scratch.page();
    let what = format!("a read of block {block} of the boot disk");
    match call(device, scratch, &what, |d| {
        d.read(disk, block, count, &[page])
    })? {
        Ok(moved) => Ok(Some(answer(scratch, moved))),
        Err(_) => Ok(None),
    }
}

/// What the boot disk's GPT says: the disk GUID its checked header names, and its
/// `moruna-system` partition as a window of its blocks, when it has one its entries hold whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    /// The header's disk GUID.
    pub guid: moruna_disk::Guid,
    /// The `moruna-system` partition.
    pub system: Option<moruna_disk::Slice>,
}

/// The GPT of `disk`, the boot disk: the primary header at block 1, checked, and the entries it
/// names, read a page at a time into the scratch page and checked (`moruna_disk`). `None` when
/// the disk has no header GPT checks.
pub fn boot_table<C: Channel, S: Scratch>(
    device: &mut Storvsc<C>,
    scratch: &S,
    disk: &Disk,
) -> Result<Option<Table>, String> {
    let size = u64::from(disk.block_size());
    if size > PAGE_BYTES || disk.blocks() < 2 {
        return Ok(None);
    }
    let Some(header) = read_page(device, scratch, disk, 1, 1)? else {
        return Ok(None);
    };
    let Some(guid) = moruna_disk::disk_guid(&header) else {
        return Ok(None);
    };
    Ok(Some(Table {
        guid,
        system: system(device, scratch, disk, &header)?,
    }))
}

/// The `moruna-system` partition the checked header `header` of `disk` names, as a window of
/// its blocks: `None` when its entries are not whole and checked, or none is it.
fn system<C: Channel, S: Scratch>(
    device: &mut Storvsc<C>,
    scratch: &S,
    disk: &Disk,
    header: &[u8],
) -> Result<Option<moruna_disk::Slice>, String> {
    let size = u64::from(disk.block_size());
    let Some(entries) = moruna_disk::entries(header) else {
        return Ok(None);
    };
    let per_page = PAGE_BYTES / size;
    let mut bytes = Vec::new();
    for page in 0..entries.bytes().div_ceil(PAGE_BYTES) {
        let block = entries.lba + page * per_page;
        if block + per_page > disk.blocks() {
            return Ok(None);
        }
        match read_page(device, scratch, disk, block, per_page as u32)? {
            Some(read) => bytes.extend_from_slice(&read),
            None => return Ok(None),
        }
    }
    Ok(moruna_disk::find(&bytes, &entries, moruna_disk::SYSTEM)
        .and_then(|p| moruna_disk::Slice::of(&p, size, size, disk.blocks())))
}
