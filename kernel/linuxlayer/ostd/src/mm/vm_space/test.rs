// SPDX-License-Identifier: MPL-2.0

//! Host tests of the space's mirror in the core, on the emulated core: frames from chunks,
//! the calls a cursor makes and how few, showing and hiding, the clone a fork uses, and
//! what a refused call leaves.

use moruna_abi::map;

use super::{
    Op, VmSpace, absorb,
    calls::{self, CoreError, ProgramProcess},
    emulated::{self, Call},
    push_op,
};
use crate::{
    Error,
    mm::{
        CachePolicy, FrameAllocOptions, PAGE_SIZE, PageFlags, PageProperty, UFrame, VmIo,
        frame::{chunks_held, free_bytes},
    },
    prelude::*,
    task::disable_preempt,
};

const RW: u64 = map::READ | map::WRITE;
const R: u64 = map::READ;

fn prop(flags: PageFlags) -> PageProperty {
    PageProperty::new_user(flags, CachePolicy::Writeback)
}

fn frames(n: usize) -> Vec<UFrame> {
    FrameAllocOptions::new()
        .alloc_segment(n)
        .unwrap()
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Maps `frames` from `va` in one cursor.
fn map_at(space: &VmSpace, va: usize, frames: &[UFrame], flags: PageFlags) {
    let guard = disable_preempt();
    let mut c = space
        .cursor_mut(&guard, &(va..va + frames.len() * PAGE_SIZE))
        .unwrap();
    for f in frames {
        c.map(f.clone(), prop(flags));
    }
}

/// What `process` shows at `va`: the object's page, as its program reaches it, and its
/// protection.
fn shown(process: ProgramProcess, va: usize) -> Option<(usize, u64)> {
    let page = *emulated::pages(process).get(&va)?;
    let host = emulated::translate(process, va, false)?;
    Some((host, page.prot))
}

/// Ends an emulated process when dropped, before the frames declared ahead of it go: a
/// test that ends in a panic leaves no process mapping a freed frame.
struct EndOnDrop(ProgramProcess);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        emulated::process_end(self.0);
    }
}

fn calls_during(f: impl FnOnce()) -> calls::CallCounts {
    let before = calls::call_counts();
    f();
    calls::call_counts().since(&before)
}

#[ktest]
fn frames_are_pages_of_chunks() {
    let f = FrameAllocOptions::new().alloc_frame().unwrap();
    let (object, offset) = f.object_offset();
    assert_eq!(emulated::object_at(f.paddr()), Some((object, offset)));
    assert!(offset.is_multiple_of(PAGE_SIZE));
    let (chunks, bytes) = chunks_held();
    assert!(chunks >= 1 && bytes >= 2 << 20);
    let _ = free_bytes();

    // A page freed and taken again is zeroed; a fresh one already is.
    f.write_val(16, &0xabcd_u32).unwrap();
    let at = f.paddr();
    drop(f);
    let again = FrameAllocOptions::new().alloc_frame().unwrap();
    assert_eq!(again.read_val::<u32>(16).unwrap(), 0);
    let _ = at;

    // Chunks grow as frames are taken; of two chunks that empty, one is given back.
    let start = chunks_held().0;
    let mut segments = Vec::new();
    while chunks_held().0 < start + 2 {
        assert!(segments.len() < 256, "no chunk was made");
        segments.push(FrameAllocOptions::new().alloc_segment(1024).unwrap());
    }
    let peak = chunks_held().0;
    drop(segments);
    assert!(chunks_held().0 < peak);

    assert_eq!(
        FrameAllocOptions::new().alloc_segment(0).unwrap_err(),
        Error::InvalidArgs
    );
    assert_eq!(
        FrameAllocOptions::new().alloc_segment(1 << 17).unwrap_err(),
        Error::NoMemory
    );
    emulated::refuse_next(Call::Object, CoreError::QuotaExceeded);
    let refused = FrameAllocOptions::new().alloc_segment(60_000);
    emulated::clear_refusals();
    assert_eq!(refused.unwrap_err(), Error::NoMemory);

    // Old bytes stay in pages not zeroed.
    let raw = FrameAllocOptions::new()
        .zeroed(false)
        .alloc_segment(3)
        .unwrap();
    assert_eq!(raw.into_iter().count(), 3);
}

#[ktest]
fn a_shown_space_maps_what_its_record_holds() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    assert_eq!(space.shown_in(), Some(process));
    let run = frames(3);
    run[1].write_val(0, &7u64).unwrap();
    let va = 0x40_0000;

    // Three pages of one chunk, in a row: one call.
    let made = calls_during(|| map_at(&space, va, &run, PageFlags::RW));
    assert_eq!((made.maps, made.mapped_pages), (1, 3));
    for (i, f) in run.iter().enumerate() {
        let (host, prot) = shown(process, va + i * PAGE_SIZE).unwrap();
        assert_eq!((host, prot), (f.paddr(), RW));
    }
    let byte = emulated::translate(process, va + PAGE_SIZE, false).unwrap();
    // SAFETY: the host address is the frame's, alive while `run` is.
    assert_eq!(unsafe { *(byte as *const u64) }, 7);

    // Read only, all three: one call.
    let made = calls_during(|| {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + 3 * PAGE_SIZE)).unwrap();
        while c
            .protect_next(va + 3 * PAGE_SIZE - c.virt_addr(), |f, _| *f = PageFlags::R)
            .is_some()
        {}
    });
    assert_eq!(made.protects, 1);
    assert_eq!(shown(process, va).unwrap().1, R);
    assert!(emulated::translate(process, va, true).is_none());

    // A change that leaves the protection as it is makes no call.
    let made = calls_during(|| {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + PAGE_SIZE)).unwrap();
        c.protect_next(PAGE_SIZE, |f, _| *f |= PageFlags::ACCESSED);
    });
    assert_eq!(made.calls(), 0);

    // An unmap of any length, holes included: one call, and the program loses them.
    let made = calls_during(|| {
        let guard = disable_preempt();
        let mut c = space
            .cursor_mut(&guard, &(va - PAGE_SIZE..va + 8 * PAGE_SIZE))
            .unwrap();
        assert_eq!(c.unmap(9 * PAGE_SIZE), 3);
    });
    assert_eq!(made.unmaps, 1);
    assert!(emulated::pages(process).is_empty());

    // Nothing shown, nothing to unmap.
    let made = calls_during(|| {
        let guard = disable_preempt();
        space
            .cursor_mut(&guard, &(va..va + PAGE_SIZE))
            .unwrap()
            .unmap(PAGE_SIZE);
    });
    assert_eq!(made.calls(), 0);
    emulated::process_end(process);
}

#[ktest]
fn fault_around_is_one_call_and_single_faults_one_each() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let va = 0x80_0000;
    let run = frames(16);
    let around = calls_during(|| map_at(&space, va, &run, PageFlags::R));
    assert_eq!((around.maps, around.mapped_pages), (1, 16));

    let single = calls_during(|| {
        for (i, f) in frames(16).into_iter().enumerate() {
            map_at(&space, va + (16 + i) * PAGE_SIZE, &[f], PageFlags::R);
        }
    });
    assert_eq!((single.maps, single.mapped_pages), (16, 16));
    assert_eq!(emulated::pages(process).len(), 32);
    emulated::process_end(process);
    // An ended process maps nothing; hiding there finds nothing to unmap.
    space.hide();
}

#[ktest]
fn copy_on_write_replaces_a_page_in_one_call() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let va = 0x10_0000;
    let old = frames(1);
    map_at(&space, va, &old, PageFlags::R);
    let new = frames(1);
    let made = calls_during(|| {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + PAGE_SIZE)).unwrap();
        c.unmap(PAGE_SIZE);
        c.jump(va).unwrap();
        c.map(new[0].clone(), prop(PageFlags::RW));
    });
    assert_eq!((made.maps, made.unmaps), (1, 0));
    assert_eq!(shown(process, va), Some((new[0].paddr(), RW)));
    // The old frame was kept until the call was made, then let go.
    assert_eq!(old[0].reference_count(), 1);
    emulated::process_end(process);
}

#[ktest]
fn activation_shows_one_space_per_process() {
    let own = emulated::own_process();
    let a = Arc::new(VmSpace::new());
    let b = Arc::new(VmSpace::new());
    let fa = frames(2);
    let fb = frames(1);
    map_at(&a, 0x20_0000, &fa, PageFlags::RW);
    map_at(&b, 0x30_0000, &fb, PageFlags::R);
    assert!(emulated::pages(own).is_empty() || a.shown_in().is_none());

    a.activate();
    assert_eq!(a.shown_in(), Some(own));
    assert_eq!(shown(own, 0x20_0000 + PAGE_SIZE).unwrap().0, fa[1].paddr());
    // Again: nothing to do.
    let made = calls_during(|| a.activate());
    assert_eq!(made.calls(), 0);

    // Another space activated on the thread takes its process: the first is hidden.
    b.activate();
    assert_eq!(a.shown_in(), None);
    assert_eq!(b.shown_in(), Some(own));
    let pages = emulated::pages(own);
    assert_eq!(pages.len(), 1);
    assert!(pages.contains_key(&0x30_0000));

    // A space moves between processes.
    let other = emulated::process_create();
    b.show_in(other);
    assert!(emulated::pages(own).is_empty());
    assert_eq!(emulated::pages(other).len(), 1);
    b.show_in(other);
    b.hide();
    assert!(emulated::pages(other).is_empty());
    b.hide();
    assert_eq!(b.shown_in(), None);

    // A shown space that ends unmaps its pages.
    let c = Arc::new(VmSpace::new());
    map_at(&c, 0x20_0000, &fa, PageFlags::RW);
    c.show_in(own);
    assert_eq!(emulated::pages(own).len(), 2);
    drop(c);
    assert!(emulated::pages(own).is_empty());
    emulated::process_end(other);
}

/// Copies `parent`'s record into `child` as `vm/`'s fork does: every page loses write
/// access on both sides.
fn fork_record(parent: &VmSpace, child: &VmSpace, range: core::ops::Range<usize>) {
    let guard = disable_preempt();
    let mut from = parent.cursor_mut(&guard, &range).unwrap();
    let mut to = child.cursor_mut(&guard, &range).unwrap();
    while let Some(va) = from.find_next(range.end - from.virt_addr()) {
        let (frame, mut p) = match from.query().unwrap() {
            (_, Some(super::VmQueriedItem::MappedRam { frame, prop })) => ((*frame).clone(), prop),
            _ => unreachable!(),
        };
        from.protect_next(range.end - va, |f, _| *f -= PageFlags::W);
        p.flags -= PageFlags::W;
        to.jump(va).unwrap();
        to.map(frame, p);
        if from.virt_addr() >= range.end {
            break;
        }
    }
}

#[ktest]
fn a_fork_shows_its_child_through_the_core_clone() {
    let parent_process = emulated::process_create();
    let parent = Arc::new(VmSpace::new());
    parent.show_in(parent_process);
    let base = 0x50_0000;
    let range = base..base + 64 * PAGE_SIZE;
    map_at(&parent, base, &frames(4), PageFlags::RW);
    map_at(&parent, base + 8 * PAGE_SIZE, &frames(2), PageFlags::RW);

    let child = Arc::new(VmSpace::new());
    let made = calls_during(|| fork_record(&parent, &child, range.clone()));
    // The parent's protections: one call a run.
    assert_eq!(made.protects, 2);

    let made = calls_during(|| {
        let cloned = child
            .show_cloned(&parent, |p| {
                emulated::process_clone(p).map_err(calls::to_error)
            })
            .unwrap();
        assert_eq!(emulated::pages(cloned), emulated::pages(parent_process));
        assert_eq!(child.shown_in(), Some(cloned));
    });
    // The clone already shows every page: no call.
    assert_eq!(made.calls(), 0);
    for (_, page) in emulated::pages(child.shown_in().unwrap()) {
        assert_eq!(page.prot, R);
    }

    // A child that differs: a page it lacks goes, one it adds comes, one with more access
    // is raised.
    let other = Arc::new(VmSpace::new());
    fork_record(&parent, &other, range.clone());
    {
        let guard = disable_preempt();
        let mut c = other
            .cursor_mut(&guard, &(base..base + 32 * PAGE_SIZE))
            .unwrap();
        c.unmap(PAGE_SIZE);
        c.jump(base + 20 * PAGE_SIZE).unwrap();
        c.map(frames(1).remove(0), prop(PageFlags::R));
        c.jump(base + 8 * PAGE_SIZE).unwrap();
        c.protect_next(PAGE_SIZE, |f, _| *f |= PageFlags::W);
    }
    let made = calls_during(|| {
        let cloned = other
            .show_cloned(&parent, |p| {
                emulated::process_clone(p).map_err(calls::to_error)
            })
            .unwrap();
        let pages = emulated::pages(cloned);
        assert!(!pages.contains_key(&base));
        assert!(pages.contains_key(&(base + 20 * PAGE_SIZE)));
        assert_eq!(pages[&(base + 8 * PAGE_SIZE)].prot, RW);
    });
    assert_eq!((made.maps, made.unmaps, made.protects), (1, 1, 1));

    // Refusals: a parent shown nowhere, a child shown already, a clone that fails.
    let lone = VmSpace::new();
    let third = Arc::new(VmSpace::new());
    assert_eq!(
        third.show_cloned(&lone, |p| Ok(p)).unwrap_err(),
        Error::InvalidArgs
    );
    assert_eq!(
        child.show_cloned(&parent, |p| Ok(p)).unwrap_err(),
        Error::InvalidArgs
    );
    assert_eq!(
        third
            .show_cloned(&parent, |_| Err(Error::NoMemory))
            .unwrap_err(),
        Error::NoMemory
    );
    // A clone whose process value shows another space already takes it over.
    let squatter = Arc::new(VmSpace::new());
    let taken = emulated::process_create();
    map_at(&squatter, 0x90_0000, &frames(1), PageFlags::R);
    squatter.show_in(taken);
    third.show_cloned(&parent, |_| Ok(taken)).unwrap();
    assert_eq!(squatter.shown_in(), None);
    assert!(!emulated::pages(taken).contains_key(&0x90_0000));

    for p in [parent_process, taken] {
        emulated::process_end(p);
    }
}

#[ktest]
fn refused_calls_lag_and_show_on_the_next_fault() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let va = 0x60_0000;
    let f = frames(1);

    emulated::refuse_next(Call::Map, CoreError::QuotaExceeded);
    let made = calls_during(|| map_at(&space, va, &f, PageFlags::RW));
    assert_eq!(made.lagging, 1);
    assert!(shown(process, va).is_none());
    // The program faults there: the page shows.
    assert_eq!(space.serve_lagging(va + 8, true, false), Ok(true));
    assert_eq!(shown(process, va), Some((f[0].paddr(), RW)));
    // Shown already: a fault raced with another thread's.
    assert_eq!(space.serve_lagging(va, false, false), Ok(true));
    // Not the record's to serve: no page, or no such access.
    assert_eq!(space.serve_lagging(va + PAGE_SIZE, false, false), Ok(false));
    assert_eq!(space.serve_lagging(va, false, true), Ok(false));
    // Refused again at the fault: the program gets the error.
    emulated::refuse_next(Call::Map, CoreError::QuotaExceeded);
    let g = frames(1);
    map_at(&space, va + PAGE_SIZE, &g, PageFlags::R);
    emulated::refuse_next(Call::Map, CoreError::NoMemory);
    assert_eq!(
        space.serve_lagging(va + PAGE_SIZE, false, false),
        Err(Error::NoMemory)
    );
    assert_eq!(space.serve_lagging(va + PAGE_SIZE, false, false), Ok(true));

    // A refused protection: the pages go, and show again with the new protection.
    emulated::refuse_next(Call::Protect, CoreError::QuotaExceeded);
    {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + PAGE_SIZE)).unwrap();
        c.protect_next(PAGE_SIZE, |f, _| *f = PageFlags::R);
    }
    assert!(shown(process, va).is_none());
    assert_eq!(space.serve_lagging(va, true, false), Ok(false));
    assert_eq!(space.serve_lagging(va, false, false), Ok(true));
    assert_eq!(shown(process, va).unwrap().1, R);

    // A refused replay at `show_in` lags too.
    let other = emulated::process_create();
    emulated::refuse_next(Call::Map, CoreError::NoMemory);
    space.show_in(other);
    assert!(emulated::pages(other).is_empty());
    assert_eq!(space.serve_lagging(va, false, false), Ok(true));
    assert_eq!(emulated::pages(other).len(), 1);

    // A space shown nowhere serves nothing.
    space.hide();
    assert_eq!(space.serve_lagging(va, false, false), Ok(false));
    emulated::process_end(process);
    emulated::process_end(other);
}

#[ktest]
#[should_panic(expected = "refused to unmap")]
fn a_refused_unmap_ends_the_personality() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let f = frames(1);
    let _end = EndOnDrop(process);
    map_at(&space, 0x70_0000, &f, PageFlags::R);
    emulated::refuse_next(Call::Unmap, CoreError::QuotaExceeded);
    let guard = disable_preempt();
    let mut c = space
        .cursor_mut(&guard, &(0x70_0000..0x70_0000 + PAGE_SIZE))
        .unwrap();
    c.unmap(PAGE_SIZE);
    drop(c);
}

#[ktest]
#[should_panic(expected = "take a program's pages away")]
fn a_refused_protection_that_cannot_unmap_ends_the_personality() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let f = frames(1);
    let _end = EndOnDrop(process);
    map_at(&space, 0x70_0000, &f, PageFlags::RW);
    emulated::refuse_next(Call::Protect, CoreError::QuotaExceeded);
    emulated::refuse_next(Call::Unmap, CoreError::QuotaExceeded);
    let guard = disable_preempt();
    let mut c = space
        .cursor_mut(&guard, &(0x70_0000..0x70_0000 + PAGE_SIZE))
        .unwrap();
    c.protect_next(PAGE_SIZE, |f, _| *f = PageFlags::R);
    drop(c);
}

#[ktest]
#[should_panic(expected = "unmap a hidden space")]
fn a_refused_hide_ends_the_personality() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let f = frames(1);
    let _end = EndOnDrop(process);
    map_at(&space, 0x70_0000, &f, PageFlags::R);
    emulated::refuse_next(Call::Unmap, CoreError::BadHandle);
    space.hide();
}

#[ktest]
fn writable_and_executable_pages_show_one_side_at_a_time() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let va = 0xa0_0000;
    map_at(&space, va, &frames(1), PageFlags::RWX);
    assert_eq!(shown(process, va).unwrap().1, RW);
    assert_eq!(space.serve_lagging(va, false, true), Ok(true));
    assert_eq!(shown(process, va).unwrap().1, map::READ | map::EXECUTE);
    // A change of protection keeps the side shown.
    {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + PAGE_SIZE)).unwrap();
        c.protect_next(PAGE_SIZE, |f, _| *f |= PageFlags::DIRTY);
    }
    assert_eq!(shown(process, va).unwrap().1, map::READ | map::EXECUTE);
    assert_eq!(space.serve_lagging(va, true, false), Ok(true));
    assert_eq!(shown(process, va).unwrap().1, RW);
    emulated::process_end(process);
}

#[ktest]
fn a_long_unmap_lets_frames_go_as_it_goes() {
    let process = emulated::process_create();
    let space = Arc::new(VmSpace::new());
    space.show_in(process);
    let va = 0x100_0000;
    let n = super::HOLD_MAX + 64;
    let run = frames(n);
    map_at(&space, va, &run, PageFlags::R);
    assert_eq!(emulated::pages(process).len(), n);
    let made = calls_during(|| {
        let guard = disable_preempt();
        let mut c = space.cursor_mut(&guard, &(va..va + n * PAGE_SIZE)).unwrap();
        for _ in 0..n {
            c.unmap(PAGE_SIZE);
        }
    });
    // One call for the run up to the limit, then one for the rest.
    assert_eq!(made.unmaps, 2);
    assert!(emulated::pages(process).is_empty());
    emulated::process_end(process);
}

#[ktest]
fn runs_merge_into_one_call() {
    let m = |va, offset, prot| Op::Map {
        va,
        object: 1,
        offset,
        len: PAGE_SIZE,
        prot,
    };
    let mut op = m(0x1000, 0, R);
    assert!(absorb(&mut op, &m(0x2000, PAGE_SIZE, R)));
    assert!(!absorb(&mut op, &m(0x3000, 0, R)));
    assert!(!absorb(&mut op, &m(0x3000, 2 * PAGE_SIZE, RW)));
    assert!(!absorb(
        &mut op,
        &Op::Map {
            va: 0x3000,
            object: 2,
            offset: 2 * PAGE_SIZE,
            len: PAGE_SIZE,
            prot: R
        }
    ));
    let mut un = Op::Unmap {
        va: 0x1000,
        len: PAGE_SIZE,
    };
    assert!(absorb(&mut un, &m(0x1000, 0, RW)));
    assert_eq!(un, m(0x1000, 0, RW));
    let mut un = Op::Unmap {
        va: 0x1000,
        len: PAGE_SIZE,
    };
    assert!(absorb(
        &mut un,
        &Op::Unmap {
            va: 0x2000,
            len: PAGE_SIZE
        }
    ));
    assert!(!absorb(&mut un, &m(0x5000, 0, R)));
    let mut p = Op::Protect {
        va: 0x1000,
        len: PAGE_SIZE,
        prot: R,
    };
    assert!(absorb(
        &mut p,
        &Op::Protect {
            va: 0x2000,
            len: PAGE_SIZE,
            prot: R
        }
    ));
    assert!(!absorb(
        &mut p,
        &Op::Protect {
            va: 0x3000,
            len: PAGE_SIZE,
            prot: RW
        }
    ));
    let mut ops = Vec::new();
    push_op(&mut ops, m(0x1000, 0, R));
    push_op(&mut ops, m(0x2000, PAGE_SIZE, R));
    push_op(&mut ops, m(0x9000, 0, R));
    assert_eq!(ops.len(), 2);
}

#[ktest]
fn protections_errors_and_counts() {
    use calls::prot_of;
    assert_eq!(prot_of(false, false, false, false), 0);
    assert_eq!(prot_of(true, false, false, false), R);
    assert_eq!(prot_of(true, true, false, false), RW);
    assert_eq!(prot_of(false, false, true, false), map::READ | map::EXECUTE);
    assert_eq!(prot_of(true, true, true, false), RW);
    assert_eq!(prot_of(true, true, true, true), map::READ | map::EXECUTE);
    for (core, ours) in [
        (CoreError::QuotaExceeded, Error::NoMemory),
        (CoreError::OutOfRange, Error::InvalidArgs),
        (CoreError::BadHandle, Error::AccessDenied),
        (CoreError::BadState, Error::NotEnoughResources),
    ] {
        assert_eq!(calls::to_error(core), ours);
    }
    let a = calls::call_counts();
    assert_eq!(a.since(&a).calls(), 0);
    let p = ProgramProcess::from_raw(9);
    assert_eq!(p.raw(), 9);
    assert!(alloc::format!("{:?}", VmSpace::new()).contains("VmSpace"));
}

#[ktest]
fn the_emulated_core_checks_as_the_core_does() {
    use emulated::{map as m, protect, unmap};
    let p = emulated::process_create();
    let f = frames(2);
    let (object, offset) = f[0].object_offset();
    let fixed = RW | map::FIXED;
    assert_eq!(
        m(p, 0x1001, object, offset, PAGE_SIZE, fixed),
        Err(CoreError::InvalidArgs)
    );
    assert_eq!(
        m(p, 0, object, offset, PAGE_SIZE, fixed),
        Err(CoreError::OutOfRange)
    );
    assert_eq!(
        m(p, 0x1000, object, offset, PAGE_SIZE, RW | map::EXECUTE),
        Err(CoreError::InvalidArgs)
    );
    assert_eq!(
        m(p, 0x1000, object, offset, PAGE_SIZE, 1 << 40),
        Err(CoreError::InvalidArgs)
    );
    assert_eq!(
        m(p, 0x1000, 0, 0, PAGE_SIZE, fixed),
        Err(CoreError::BadHandle)
    );
    assert_eq!(
        m(p, 0x1000, object, 1 << 40, PAGE_SIZE, fixed),
        Err(CoreError::OutOfRange)
    );
    let gone = ProgramProcess::from_raw(u32::MAX);
    assert_eq!(
        m(gone, 0x1000, object, offset, PAGE_SIZE, fixed),
        Err(CoreError::BadHandle)
    );
    assert_eq!(m(p, 0x1000, object, offset, PAGE_SIZE, RW), Ok(()));
    assert_eq!(
        m(p, 0x1000, object, offset, PAGE_SIZE, RW),
        Err(CoreError::BadState)
    );
    assert_eq!(
        protect(p, 0x1000, 2 * PAGE_SIZE, R),
        Err(CoreError::OutOfRange)
    );
    assert_eq!(
        protect(p, 0x1000, PAGE_SIZE, 1 << 9),
        Err(CoreError::InvalidArgs)
    );
    assert_eq!(protect(p, 0x1000, PAGE_SIZE, R), Ok(()));
    assert!(emulated::translate(p, 0x1000, true).is_none());
    assert!(emulated::translate(p, 0x1000, false).is_some());
    assert!(emulated::translate(gone, 0x1000, false).is_none());
    assert_eq!(unmap(p, 0x1000, 0), Err(CoreError::InvalidArgs));
    assert_eq!(unmap(p, 0x1000, PAGE_SIZE), Ok(()));
    assert!(emulated::pages(gone).is_empty());
    assert_eq!(
        emulated::process_clone(gone).unwrap_err(),
        CoreError::BadHandle
    );
    assert!(emulated::object_at(1).is_none());
    assert!(emulated::object_create(0).is_err());
    emulated::process_end(p);
    assert_eq!(
        m(p, 0x1000, object, offset, PAGE_SIZE, fixed),
        Err(CoreError::BadState)
    );
}

#[ktest]
#[should_panic(expected = "released while process")]
fn the_emulated_core_catches_an_object_freed_while_mapped() {
    let p = emulated::process_create();
    let _end = EndOnDrop(p);
    let made = emulated::object_create(PAGE_SIZE).unwrap();
    emulated::map(p, 0x1000, made.raw, 0, PAGE_SIZE, R).unwrap();
    emulated::object_release(made.raw, made.addr, PAGE_SIZE);
}
