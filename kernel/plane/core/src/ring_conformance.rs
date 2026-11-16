//! Ring mode's conformance (`architecture/data-os.md` §5, SDD 29 c): DP-I1 to DP-I9 for a ring
//! task, in both ways a record leaves the task's reach, on the model of the kernel's calls.
//!
//! Each case opens ring tasks on a fresh [`RingPlane`], runs [`RingTaskModel`]s against it, and
//! judges what the engine saw and what each task saw. The figures a case measures (calls and
//! copies per batch) come back in [`Measure`].

use alloc::{format, string::String, vec, vec::Vec};

use crate::conformance::{Ending, Fixtures, Results};
use crate::model::Model;
use crate::model_ring::{Behave, RingTaskModel, model};
use crate::port::{BatchId, CallId, DataPlane, Event, PlaneError, TaskId};
use crate::ring::{Mode, Shared, layout};
use crate::ringed::{Region, RegionId, RingPlane, RingShape, View};

type Plane = RingPlane<Model>;

/// What a lockstep or pipelined run cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    pub batches: u64,
    /// Calls the engine made into the kernel while batches moved.
    pub calls: u64,
    /// Waits the task called.
    pub waits: u64,
    /// Revokes and regrants the engine called.
    pub revokes: u64,
    /// Rings objects handed back.
    pub generations: u64,
}

const MIB: u64 = 1 << 20;

fn plane(protect: bool) -> Plane {
    let mut p = RingPlane::new(model(protect), 64 * MIB);
    p.spins = 1;
    p
}

fn shape(windows: u64) -> RingShape {
    RingShape {
        region: Region::New(MIB),
        windows,
        window_bytes: 64 * 1024,
    }
}

fn open(
    p: &mut Plane,
    shape: RingShape,
    behave: Behave,
) -> Result<(TaskId, RingTaskModel), String> {
    let (task, port) = p.open_ring(shape).map_err(|e| format!("open: {e:?}"))?;
    let slots = p.ring_slots(task).map_err(|e| format!("slots: {e:?}"))?;
    let t = RingTaskModel::new([slots[0], slots[1]], port, behave);
    Ok((task, t))
}

/// Runs the tasks and the engine until neither moves; the engine's events.
fn drive(p: &mut Plane, tasks: &mut [&mut RingTaskModel]) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
    for _ in 0..10_000 {
        let mut moved = false;
        for t in tasks.iter_mut() {
            moved |= t.step();
        }
        while let Some(e) = p.next(0).map_err(|e| format!("next: {e:?}"))? {
            events.push(e);
            moved = true;
        }
        if !moved {
            return Ok(events);
        }
    }
    Err(String::from("the plane and its tasks never settled"))
}

fn load(p: &mut Plane, region: RegionId, f: &Fixtures) -> Result<BatchId, String> {
    let good = f.good.clone();
    p.load_into(region, good.len() as u64, |v, at| v.write(at, &good))
        .map(|(b, rows)| {
            debug_assert_eq!(rows, f.rows);
            b
        })
        .map_err(|e| format!("load_into: {e:?}"))
}

fn puts(events: &[Event]) -> Vec<(TaskId, BatchId, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Put {
                task, batch, rows, ..
            } => Some((*task, *batch, *rows)),
            _ => None,
        })
        .collect()
}

fn count(events: &[Event], f: impl Fn(&Event) -> bool) -> usize {
    events.iter().filter(|e| f(e)).count()
}

fn same<T: PartialEq + core::fmt::Debug>(got: T, want: T, what: &str) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn bytes_of(p: &Plane, batch: BatchId) -> Result<Vec<u8>, String> {
    let (view, at, len) = p
        .batch_view(batch)
        .ok_or_else(|| format!("{batch:?} is not a ring batch"))?;
    let mut out = vec![0u8; len as usize];
    view.read(at, &mut out)
        .map_err(|e| format!("read: {e:?}"))?;
    Ok(out)
}

fn empty(p: &Plane, what: &str) -> Result<(), String> {
    let r = p.residency();
    same((r.batches, r.shares, r.resident), (0, 0, 0), what)
}

/// Every case, in both modes.
pub fn run_all(f: &Fixtures) -> Results {
    let mut r: Results = Vec::new();
    for protect in [true, false] {
        let m = if protect { " (protect)" } else { " (unmap)" };
        let _ = m;
        r.push((
            if protect {
                "dp_i1_lockstep_protect"
            } else {
                "dp_i1_lockstep_unmap"
            },
            lockstep(f, protect, 16).map(|_| ()),
        ));
        r.push((
            if protect {
                "dp_i1_pipelined_protect"
            } else {
                "dp_i1_pipelined_unmap"
            },
            pipelined(f, protect, 32).map(|_| ()),
        ));
        r.push((
            if protect {
                "dp_i3_checked_unwritable_protect"
            } else {
                "dp_i3_checked_unwritable_unmap"
            },
            checked_unwritable(f, protect),
        ));
        r.push((
            if protect {
                "dp_i6_fan_out_protect"
            } else {
                "dp_i6_fan_out_unmap"
            },
            fan_out(f, protect),
        ));
        r.push((
            if protect {
                "dp_i8_crash_protect"
            } else {
                "dp_i8_crash_unmap"
            },
            crash(f, protect),
        ));
        r.push((
            if protect {
                "dp_i9_refusals_protect"
            } else {
                "dp_i9_refusals_unmap"
            },
            refusals(f, protect),
        ));
    }
    r.push(("dp_i4_region_read_only", region_read_only(f)));
    r.push(("dp_i7_bounded", bounded(f)));
    r.push(("dp_i2_ports_and_waits", ports_and_waits(f)));
    r.push(("c4_zeroed_before_seen", zeroed(f)));
    r.push(("c6_restart_fresh", restart(f)));
    r
}

/// Every case, and an error naming each that failed.
pub fn check(f: &Fixtures) -> Result<usize, String> {
    let results = run_all(f);
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    if failed.is_empty() {
        Ok(results.len())
    } else {
        Err(failed.join("\n"))
    }
}

/// DP-I1, lockstep (one batch in flight, as `apply` is): each batch is read by the task in
/// place in the region the engine wrote, and each record is checked in place in the window the
/// task wrote. No byte moves between them but the task's own write.
pub fn lockstep(f: &Fixtures, protect: bool, n: u64) -> Result<Measure, String> {
    let mut p = plane(protect);
    let (task, mut t) = open(&mut p, shape(4), Behave::Echo)?;
    same(
        p.mode(task),
        Some(if protect { Mode::Protect } else { Mode::Unmap }),
        "mode",
    )?;
    let region = p.region_of(task).ok_or("no region")?;
    drive(&mut p, &mut [&mut t])?;
    let calls0 = p.plane.calls.calls;
    let waits0 = t.waits;
    for i in 0..n {
        let b = load(&mut p, region, f)?;
        p.offer(task, 0, b)
            .map_err(|e| format!("offer {i}: {e:?}"))?;
        p.drop_batch(b).map_err(|e| format!("drop {i}: {e:?}"))?;
        let events = drive(&mut p, &mut [&mut t])?;
        let put = puts(&events);
        same(put.len(), 1, "one record a batch")
            .map_err(|e| format!("batch {i}: {e}: {events:?}, task {:?}", t.ended))?;
        let (_, out, rows) = put[0];
        same(rows, f.rows, "rows")?;
        same(
            bytes_of(&p, out)?,
            f.good.clone(),
            "the record is the task's",
        )?;
        same(
            count(&events, |e| matches!(e, Event::Released { .. })),
            1,
            "the batch released",
        )?;
        p.drop_batch(out)
            .map_err(|e| format!("drop out {i}: {e:?}"))?;
        drive(&mut p, &mut [&mut t])?;
    }
    let (view, _) = p.region_view(region).ok_or("region view")?;
    same(
        t.region_ptr(),
        Some(view.base()),
        "the task maps the region the engine writes",
    )?;
    same(
        t.read.iter().all(|(_, ptr)| *ptr == view.base()),
        true,
        "every batch read in place",
    )?;
    let c = p.counters();
    let m = Measure {
        batches: n,
        calls: p.plane.calls.calls - calls0,
        waits: t.waits - waits0,
        revokes: c.revokes + c.regrants,
        generations: c.generations,
    };
    if protect {
        same(c.generations, 0, "no rings object handed back")?;
        same(
            c.revokes,
            n,
            "one revoke a group, one record a group in lockstep",
        )?;
    } else {
        same(c.revokes + c.regrants, 0, "no revoke call")?;
        same(c.generations, n, "one hand-back a group")?;
    }
    p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
    let events = drive(&mut p, &mut [&mut t])?;
    same(
        t.ended,
        Some(Ending::Exited),
        "the task read the end and exited",
    )?;
    same(
        count(&events, |e| matches!(e, Event::Ended { .. })),
        1,
        "ended once",
    )?;
    empty(&p, "nothing left")?;
    Ok(m)
}

/// DP-I1 with batches in flight: the engine posts `n` before the task runs. A group is every
/// record posted since the last look, so the revoke (or the hand-back) is paid once a group,
/// and the input side makes no call at all.
pub fn pipelined(f: &Fixtures, protect: bool, n: u64) -> Result<Measure, String> {
    let mut p = plane(protect);
    let (task, mut t) = open(&mut p, shape(n), Behave::Echo)?;
    let region = p.region_of(task).ok_or("no region")?;
    drive(&mut p, &mut [&mut t])?;
    let calls0 = p.plane.calls.calls;
    let waits0 = t.waits;
    for _ in 0..n {
        let b = load(&mut p, region, f)?;
        p.offer(task, 0, b).map_err(|e| format!("offer: {e:?}"))?;
        p.drop_batch(b).map_err(|e| format!("drop: {e:?}"))?;
    }
    let events = drive(&mut p, &mut [&mut t])?;
    let put = puts(&events);
    same(put.len() as u64, n, "every record")?;
    let calls = p.plane.calls.calls - calls0;
    for (_, b, _) in &put {
        same(bytes_of(&p, *b)?, f.good.clone(), "the record")?;
        p.drop_batch(*b).map_err(|e| format!("drop: {e:?}"))?;
    }
    let c = p.counters();
    if protect {
        same(c.revokes, 1, "one revoke for the group")?;
    } else {
        same(c.generations, 1, "one hand-back for the group")?;
    }
    if calls >= n {
        return Err(format!("{calls} calls for {n} batches in flight"));
    }
    Ok(Measure {
        batches: n,
        calls,
        waits: t.waits - waits0,
        revokes: c.revokes + c.regrants,
        generations: c.generations,
    })
}

/// DP-I3: when the engine checks a record the task can no longer write it, and a write after
/// it faults.
fn checked_unwritable(f: &Fixtures, protect: bool) -> Result<(), String> {
    let mut p = plane(protect);
    let (task, mut t) = open(&mut p, shape(2), Behave::Echo)?;
    let region = p.region_of(task).ok_or("no region")?;
    let b = load(&mut p, region, f)?;
    p.offer(task, 0, b).map_err(|e| format!("{e:?}"))?;
    let events = drive(&mut p, &mut [&mut t])?;
    same(puts(&events).len(), 1, "a record")?;
    if protect {
        same(
            t.output.borrow().read_only.is_empty(),
            false,
            "the window revoked",
        )?;
    } else {
        same(
            t.output.borrow().current.is_none(),
            false,
            "the next rings object mapped",
        )?;
    }
    same(
        t.write_after_post(),
        false,
        "a write after the check goes nowhere",
    )?;
    Ok(())
}

/// DP-I4: the region is read only to the task: a write into it faults.
fn region_read_only(f: &Fixtures) -> Result<(), String> {
    let mut p = plane(true);
    let (task, mut t) = open(&mut p, shape(2), Behave::WriteRegion)?;
    let region = p.region_of(task).ok_or("no region")?;
    let b = load(&mut p, region, f)?;
    p.offer(task, 0, b).map_err(|e| format!("{e:?}"))?;
    let events = drive(&mut p, &mut [&mut t])?;
    same(t.ended, Some(Ending::Faulted), "the write faulted")?;
    same(
        count(&events, |e| matches!(e, Event::Ended { .. })),
        1,
        "ended",
    )?;
    same(
        count(&events, |e| matches!(e, Event::Released { .. })),
        1,
        "released at its end",
    )?;
    Ok(())
}

/// DP-I6: one layout of a batch for every consumer of a shared region; its span is freed once,
/// after the last release and the engine's hold, and only then used again.
fn fan_out(f: &Fixtures, protect: bool) -> Result<(), String> {
    let mut p = plane(protect);
    let (a, mut ta) = open(&mut p, shape(2), Behave::Echo)?;
    let region = p.region_of(a).ok_or("no region")?;
    let shared = RingShape {
        region: Region::Share(region),
        ..shape(2)
    };
    let (b, mut tb) = open(&mut p, shared, Behave::Echo)?;
    let (c, mut tc) = open(&mut p, shared, Behave::Echo)?;
    let batch = load(&mut p, region, f)?;
    for t in [a, b, c] {
        p.offer(t, 0, batch).map_err(|e| format!("{e:?}"))?;
    }
    same(p.residency().shares, 3, "three shares")?;
    let events = drive(&mut p, &mut [&mut ta, &mut tb, &mut tc])?;
    same(puts(&events).len(), 3, "three records")?;
    same(
        count(&events, |e| matches!(e, Event::Released { .. })),
        3,
        "three releases",
    )?;
    same(
        count(
            &events,
            |e| matches!(e, Event::Freed { batch: x } if *x == batch),
        ),
        0,
        "held",
    )?;
    let base = p.region_view(region).ok_or("view")?.0.base();
    for t in [&ta, &tb, &tc] {
        same(t.region_ptr(), Some(base), "one region, mapped by each")?;
    }
    p.drop_batch(batch).map_err(|e| format!("{e:?}"))?;
    let mut freed = 0;
    while let Some(e) = p.next(0).map_err(|e| format!("{e:?}"))? {
        if matches!(e, Event::Freed { batch: x } if x == batch) {
            freed += 1;
        }
    }
    same(freed, 1, "freed once")?;
    for (_, out, _) in puts(&events) {
        p.drop_batch(out).map_err(|e| format!("{e:?}"))?;
    }
    // The span is the region's again: the next batch is laid out where the first was.
    let again = load(&mut p, region, f)?;
    same(
        p.batch_view(again).map(|v| v.1),
        Some(0),
        "the span used again",
    )?;
    same(
        p.open_ring(RingShape {
            region: Region::Share(region),
            ..shape(2)
        })
        .map(|_| ())
        .err(),
        Some(PlaneError::Limit("a region shared after it held a batch")),
        "a region is shared before its first batch only",
    )?;
    Ok(())
}

/// DP-I7: the input ring and the region are bounded; a post past them is refused, not queued.
fn bounded(f: &Fixtures) -> Result<(), String> {
    let mut p = plane(true);
    let (task, _t) = open(&mut p, shape(2), Behave::Echo)?;
    let region = p.region_of(task).ok_or("no region")?;
    let cap = 64;
    let mut posted = 0;
    let mut full = None;
    for _ in 0..cap + 1 {
        let b = match load(&mut p, region, f) {
            Ok(b) => b,
            Err(e) => {
                full = Some(e);
                break;
            }
        };
        match p.offer(task, 0, b) {
            Ok(()) => posted += 1,
            Err(e) => {
                full = Some(format!("{e:?}"));
                break;
            }
        }
    }
    let why = full.ok_or("nothing refused")?;
    if !(why.contains("Full") || why.contains("Budget")) {
        return Err(format!("refused with {why}"));
    }
    if posted == 0 || posted > cap {
        return Err(format!("{posted} posted"));
    }
    Ok(())
}

/// DP-I8: a task that ends gives everything back: each batch it held released, its windows
/// and objects back, and its end once.
fn crash(f: &Fixtures, protect: bool) -> Result<(), String> {
    let mut p = plane(protect);
    let (task, mut t) = open(&mut p, shape(2), Behave::Crash)?;
    let region = p.region_of(task).ok_or("no region")?;
    let mut held = Vec::new();
    for _ in 0..3 {
        let b = load(&mut p, region, f)?;
        p.offer(task, 0, b).map_err(|e| format!("{e:?}"))?;
        held.push(b);
    }
    let events = drive(&mut p, &mut [&mut t])?;
    same(t.ended, Some(Ending::Killed), "crashed")?;
    same(
        count(&events, |e| matches!(e, Event::Released { .. })),
        3,
        "every batch released",
    )?;
    same(
        count(
            &events,
            |e| matches!(e, Event::Ended { task: x } if *x == task),
        ),
        1,
        "ended once",
    )?;
    same(p.is_ring(task), false, "gone")?;
    for b in held {
        p.drop_batch(b).map_err(|e| format!("{e:?}"))?;
    }
    for (_, b, _) in puts(&events) {
        p.drop_batch(b).map_err(|e| format!("{e:?}"))?;
    }
    while p.next(0).map_err(|e| format!("{e:?}"))?.is_some() {}
    empty(&p, "everything back")?;
    same(
        p.residency().pooled,
        p.residency().made,
        "every object in the pool",
    )?;
    Ok(())
}

/// DP-I9 and DP-I3: a record the check refuses, a record in a window the task was not given,
/// and ring words that could not be true each end the task with the reason, and nothing of
/// them becomes a batch.
fn refusals(f: &Fixtures, protect: bool) -> Result<(), String> {
    for (behave, says) in [
        (Behave::Garbage, "continuation marker"),
        (Behave::StrayWindow, "not one"),
        (Behave::BadHead, "past its ring"),
    ] {
        let mut p = plane(protect);
        let (task, mut t) = open(&mut p, shape(2), behave)?;
        let region = p.region_of(task).ok_or("no region")?;
        let b = load(&mut p, region, f)?;
        p.offer(task, 0, b).map_err(|e| format!("{e:?}"))?;
        let events = drive(&mut p, &mut [&mut t])?;
        let reason = events
            .iter()
            .find_map(|e| match e {
                Event::Refused { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .ok_or_else(|| format!("{behave:?}: not refused ({events:?})"))?;
        if !reason.contains(says) {
            return Err(format!("{behave:?}: refused with {reason:?}"));
        }
        same(puts(&events).len(), 0, "nothing a batch")?;
        same(t.ended, Some(Ending::Killed), "the task ended")?;
        same(
            count(&events, |e| matches!(e, Event::Ended { .. })),
            1,
            "ended once",
        )?;
    }
    Ok(())
}

/// DP-I2 and the waits: a task's port calls other than port 0 reach the engine as calls; a
/// task with nothing to do waits on port 0 and is answered by the next post or the end.
fn ports_and_waits(f: &Fixtures) -> Result<(), String> {
    let mut p = plane(true);
    let (task, mut t) = open(&mut p, shape(2), Behave::Echo)?;
    let region = p.region_of(task).ok_or("no region")?;
    drive(&mut p, &mut [&mut t])?;
    same(t.waits, 1, "an empty ring: one wait")?;
    same(p.counters().waits, 1, "the engine saw it")?;
    // Nothing more until the engine posts: no spinning on calls.
    drive(&mut p, &mut [&mut t])?;
    same(t.waits, 1, "still one wait")?;
    let b = load(&mut p, region, f)?;
    p.offer(task, 0, b).map_err(|e| format!("{e:?}"))?;
    let events = drive(&mut p, &mut [&mut t])?;
    same(puts(&events).len(), 1, "the post answered the wait")?;
    // A call on a real port is the engine's to answer.
    let mut message = vec![0u8; moruna_abi::plane::request::SIZE];
    moruna_abi::write_u32(
        &mut message,
        moruna_abi::plane::request::KIND,
        moruna_abi::plane::request::CALL,
    );
    moruna_abi::write_u64(&mut message, moruna_abi::plane::request::TOKEN, 99);
    moruna_abi::write_u64(&mut message, moruna_abi::plane::request::PORT, 1);
    moruna_abi::write_u64(&mut message, moruna_abi::plane::request::LEN, 2);
    message.extend_from_slice(b"hi");
    t.port.state.borrow_mut().queues[0].push_back((message, None));
    let mut call = None;
    while let Some(e) = p.next(0).map_err(|e| format!("{e:?}"))? {
        if let Event::Call {
            call: c,
            port,
            request,
        } = e
        {
            same((port, request), (1, b"hi".to_vec()), "the call")?;
            call = Some(c);
        }
    }
    let call: CallId = call.ok_or("no call")?;
    p.answer(call, Ok(b"ok")).map_err(|e| format!("{e:?}"))?;
    same(t.port.state.borrow().queues[1].len(), 1, "answered")?;
    p.finish(task, 0).map_err(|e| format!("{e:?}"))?;
    drive(&mut p, &mut [&mut t])?;
    same(t.ended, Some(Ending::Exited), "the end answered its wait")?;
    Ok(())
}

/// C4's trust boundary: memory another task has seen is zeroed before a task sees it.
fn zeroed(f: &Fixtures) -> Result<(), String> {
    let mut p = plane(true);
    let (a, mut ta) = open(&mut p, shape(2), Behave::Echo)?;
    let region = p.region_of(a).ok_or("no region")?;
    let b = load(&mut p, region, f)?;
    p.offer(a, 0, b).map_err(|e| format!("{e:?}"))?;
    p.drop_batch(b).map_err(|e| format!("{e:?}"))?;
    let events = drive(&mut p, &mut [&mut ta])?;
    for (_, out, _) in puts(&events) {
        p.drop_batch(out).map_err(|e| format!("{e:?}"))?;
    }
    p.finish(a, 0).map_err(|e| format!("{e:?}"))?;
    drive(&mut p, &mut [&mut ta])?;
    same(p.plane.zeroed(), 0, "fresh objects are the kernel's zeros")?;
    let (bt, _tb) = open(&mut p, shape(2), Behave::Echo)?;
    if p.plane.zeroed() == 0 {
        return Err(String::from(
            "a pooled object another task saw was not zeroed",
        ));
    }
    let (view, len) = p
        .region_view(p.region_of(bt).ok_or("region")?)
        .ok_or("view")?;
    let mut all = vec![0u8; len as usize];
    view.read(0, &mut all).map_err(|e| format!("{e:?}"))?;
    same(
        all.iter().all(|b| *b == 0),
        true,
        "the new task's region is zeros",
    )?;
    Ok(())
}

/// C6: a task that failed is started again as a fresh process with the same shape, on fresh
/// objects, and batches flow to it.
fn restart(f: &Fixtures) -> Result<(), String> {
    let mut p = plane(true);
    let (a, mut ta) = open(&mut p, shape(2), Behave::Crash)?;
    let region = p.region_of(a).ok_or("no region")?;
    let b = load(&mut p, region, f)?;
    p.offer(a, 0, b).map_err(|e| format!("{e:?}"))?;
    p.drop_batch(b).map_err(|e| format!("{e:?}"))?;
    let shape_a = p.shape(a).ok_or("shape")?;
    let events = drive(&mut p, &mut [&mut ta])?;
    for (_, out, _) in puts(&events) {
        p.drop_batch(out).map_err(|e| format!("{e:?}"))?;
    }
    same(p.is_ring(a), false, "the failed task is gone")?;
    let (b2, mut tb) = open(&mut p, shape_a, Behave::Echo)?;
    let region = p.region_of(b2).ok_or("no region")?;
    let again = load(&mut p, region, f)?;
    p.offer(b2, 0, again).map_err(|e| format!("{e:?}"))?;
    let events = drive(&mut p, &mut [&mut tb])?;
    same(puts(&events).len(), 1, "the fresh task answers")?;
    Ok(())
}

/// The words a task sees after the engine formats its rings object, for the Python shim's
/// golden test: the header and the first input entry of a fixed rings object.
pub fn golden() -> Vec<(usize, u64)> {
    let mut p = plane(false);
    let Ok((task, _)) = p.open_ring(RingShape {
        region: Region::New(MIB),
        windows: 2,
        window_bytes: 64 * 1024,
    }) else {
        return Vec::new();
    };
    let Some(Ok(slots)) = Some(p.ring_slots(task)) else {
        return Vec::new();
    };
    let mem = slots[1].borrow().ready.front().map(|(m, ..)| m.clone());
    let Some(mem) = mem else { return Vec::new() };
    let view = crate::model_ring::ModelMap::from_mem(&mem);
    let words = [
        layout::MAGIC,
        layout::VERSION,
        layout::REGION_BYTES,
        layout::IN_CAP,
        layout::OUT_CAP,
        layout::FREE_CAP,
        layout::WINDOWS,
        layout::WINDOW_BYTES,
        layout::WINDOWS_AT,
        layout::MODE,
        layout::FREE_HEAD,
    ];
    words.iter().map(|at| (*at, view.load(*at))).collect()
}
