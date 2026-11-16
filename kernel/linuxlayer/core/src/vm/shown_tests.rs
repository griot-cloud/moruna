// SPDX-License-Identifier: MPL-2.0

//! Host tests of `vm/` on the core's mappings (Moruna, stream B2-vm): Asterinas's VMAR
//! logic drives `ostd`'s `VmSpace`, which shows each space in a process of an emulated
//! core. Each test checks what the program would see there: faults, fault-around,
//! protection, unmapping, a new program's space, and a thread's activation. The fork
//! through the core's clone is tested with `fork.rs`'s page-table copy.

use ostd::{
    mm::vm_space::{CallCounts, ProgramProcess, call_counts, emulated},
    prelude::ktest,
};

use crate::{
    fs::pseudofs::SockFs,
    prelude::*,
    process::ProcessVm,
    thread::kernel_thread::ThreadOptions,
    vm::{
        page_cache::VmoOptions,
        perms::VmPerms,
        vmar::{PageFaultInfo, Vmar, VmarHandle, VmarMapOffset},
    },
};

/// `moruna_abi::map::READ`, and with `WRITE`.
pub(super) const R: u64 = 1;
pub(super) const RW: u64 = 3;

pub(super) fn new_vmar() -> VmarHandle {
    crate::time::clocks::init_for_ktest();
    crate::util::random::init();
    VmarHandle::new(ProcessVm::new(SockFs::new_path()))
}

/// A space shown in a new emulated process.
pub(super) fn shown_vmar() -> (VmarHandle, ProgramProcess) {
    let vmar = new_vmar();
    let process = emulated::process_create();
    vmar.vm_space().show_in(process);
    (vmar, process)
}

pub(super) fn fault(vmar: &Vmar, va: usize, write: bool) -> Result<()> {
    let perms = if write {
        VmPerms::READ | VmPerms::WRITE
    } else {
        VmPerms::READ
    };
    vmar.handle_page_fault(&PageFaultInfo::new(va, perms))
}

pub(super) fn map_anon(vmar: &Vmar, at: usize, pages: usize) -> usize {
    vmar.new_map(pages * PAGE_SIZE, VmPerms::READ | VmPerms::WRITE)
        .offset(VmarMapOffset::FixedNoReplace(at))
        .build()
        .unwrap()
}

pub(super) use emulated::{peek, poke};

pub(super) fn counted(f: impl FnOnce()) -> CallCounts {
    let before = call_counts();
    f();
    call_counts().since(&before)
}

#[ktest]
fn faults_map_through_the_core_and_unmaps_take_them_away() {
    let (vmar, p) = shown_vmar();
    let addr = map_anon(&vmar, 0x100_0000, 8);
    // Before a fault the program sees nothing.
    assert!(emulated::pages(p).is_empty());
    let made = counted(|| fault(&vmar, addr + 5, true).unwrap());
    assert_eq!((made.maps, made.mapped_pages), (1, 1));
    assert_eq!(emulated::pages(p)[&addr].prot, RW);
    assert!(poke(p, addr, 0x5a));
    assert_eq!(peek(p, addr), Some(0x5a));
    // A read of an anonymous page maps a fresh one.
    fault(&vmar, addr + PAGE_SIZE, false).unwrap();
    assert_eq!(peek(p, addr + PAGE_SIZE), Some(0));
    // Outside every mapping the fault is the program's.
    assert!(fault(&vmar, 0x900_0000, false).is_err());

    // munmap of the whole mapping: one call.
    let made = counted(|| vmar.remove_mapping(addr..addr + 8 * PAGE_SIZE));
    assert_eq!(made.unmaps, 1);
    assert!(emulated::pages(p).is_empty());
    emulated::process_end(p);
}

#[ktest]
fn fault_around_maps_its_window_in_few_calls() {
    let (vmar, p) = shown_vmar();
    let vmo = VmoOptions::new(32 * PAGE_SIZE).alloc().unwrap();
    let bytes = vec![7u8; 32 * PAGE_SIZE];
    vmo.write(0, &mut VmReader::from(&bytes[..]).to_fallible())
        .unwrap();
    let addr = vmar
        .new_map(32 * PAGE_SIZE, VmPerms::READ)
        .vmo(vmo)
        .handle_page_faults_around()
        .offset(VmarMapOffset::FixedNoReplace(0x200_0000))
        .build()
        .unwrap();
    let made = counted(|| fault(&vmar, addr + 3 * PAGE_SIZE, false).unwrap());
    // The window of 16 pages; its frames were taken in a row, so in far fewer calls.
    assert_eq!(made.mapped_pages, 16);
    assert!(made.maps < 16, "fault-around took {} calls", made.maps);
    assert_eq!(emulated::pages(p).len(), 16);
    assert_eq!(peek(p, addr + 15 * PAGE_SIZE), Some(0x0707_0707_0707_0707));
    for (_, page) in emulated::pages(p) {
        assert_eq!(page.prot, R);
    }
    // A fault in the next window: one more run.
    let made = counted(|| fault(&vmar, addr + 20 * PAGE_SIZE, false).unwrap());
    assert_eq!(made.mapped_pages, 16);
    emulated::process_end(p);
}

#[ktest]
fn protection_and_a_new_program_show_through() {
    let (vmar, p) = shown_vmar();
    let addr = map_anon(&vmar, 0x400_0000, 4);
    for i in 0..4 {
        fault(&vmar, addr + i * PAGE_SIZE, true).unwrap();
    }
    // mprotect of the middle two: one call.
    let made = counted(|| {
        vmar.protect(VmPerms::READ, addr + PAGE_SIZE..addr + 3 * PAGE_SIZE)
            .unwrap()
    });
    assert_eq!(made.protects, 1);
    let pages = emulated::pages(p);
    assert_eq!(
        [0, 1, 2, 3].map(|i| pages[&(addr + i * PAGE_SIZE)].prot),
        [RW, R, R, RW]
    );
    // A write there is the program's fault now.
    assert!(fault(&vmar, addr + PAGE_SIZE, true).is_err());

    // A new program's space (execve) takes the process: only its pages show.
    let next = new_vmar();
    let at = map_anon(&next, 0x500_0000, 1);
    fault(&next, at, true).unwrap();
    next.vm_space().show_in(p);
    let pages = emulated::pages(p);
    assert_eq!(pages.len(), 1);
    assert!(pages.contains_key(&at));
    assert_eq!(vmar.vm_space().shown_in(), None);
    emulated::process_end(p);
}

#[ktest]
fn a_thread_shows_its_space_in_its_own_process() {
    let vmar = new_vmar();
    let addr = map_anon(&vmar, 0x600_0000, 2);
    let worker_vmar = vmar.clone_arc();
    let seen = Arc::new(Mutex::new(None));
    let seen_in = seen.clone();
    let worker = ThreadOptions::new(move || {
        let own = emulated::own_process();
        let space = worker_vmar.vm_space();
        // As a switch to the thread does (`thread::post_schedule_handler`).
        space.activate();
        let data = [9u8; 2 * PAGE_SIZE];
        let mut w = space.writer(addr, 2 * PAGE_SIZE).unwrap();
        let written = w.write_fallible(&mut VmReader::from(&data[..]).to_fallible());
        let mut back = [0u8; 8];
        let mut r = space.reader(addr + PAGE_SIZE, 8).unwrap();
        let _ = r.read_fallible(&mut VmWriter::from(&mut back[..]).to_fallible());
        *seen_in.lock() = Some((
            written.ok(),
            space.shown_in() == Some(own),
            emulated::pages(own).len(),
            back,
        ));
    })
    .vmar(vmar.clone_arc())
    .spawn();
    worker.join();
    assert_eq!(
        seen.lock().take().unwrap(),
        (Some(2 * PAGE_SIZE), true, 2, [9u8; 8])
    );
}

#[ktest]
fn a_page_the_core_lags_on_shows_on_the_next_fault() {
    let (vmar, p) = shown_vmar();
    let addr = map_anon(&vmar, 0x700_0000, 2);
    // The core refuses the map: the record has the page, the program does not.
    emulated::refuse_next(emulated::Call::Map, emulated::CoreError::QuotaExceeded);
    fault(&vmar, addr, true).unwrap();
    assert!(emulated::pages(p).is_empty());
    // The program faults again: the page shows, with no new frame.
    let made = counted(|| fault(&vmar, addr, true).unwrap());
    assert_eq!((made.maps, made.mapped_pages), (1, 1));
    assert_eq!(emulated::pages(p)[&addr].prot, RW);
    // Refused at the fault too: the program gets the error.
    emulated::refuse_next(emulated::Call::Map, emulated::CoreError::QuotaExceeded);
    fault(&vmar, addr + PAGE_SIZE, true).unwrap();
    emulated::refuse_next(emulated::Call::Map, emulated::CoreError::NoMemory);
    assert!(fault(&vmar, addr + PAGE_SIZE, true).is_err());

    // Writable and executable: the core shows one side, and an instruction fetch turns it.
    let code = vmar
        .new_map(PAGE_SIZE, VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC)
        .offset(VmarMapOffset::FixedNoReplace(0x710_0000))
        .build()
        .unwrap();
    fault(&vmar, code, true).unwrap();
    assert_eq!(emulated::pages(p)[&code].prot, RW);
    vmar.handle_page_fault(&PageFaultInfo::new(code, VmPerms::EXEC))
        .unwrap();
    assert_eq!(emulated::pages(p)[&code].prot, R | 4);
    emulated::process_end(p);
}
