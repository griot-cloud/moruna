//! DP-T1 to DP-T9 (SDD 29 k): the engine's side of the data plane on a host, over a model
//! of the kernel's calls in which each test plays the task; and the port's conformance suite
//! (KA2) against the same model.

extern crate std;

use alloc::{format, rc::Rc, string::String, vec, vec::Vec};

use moruna_abi::{
    Error,
    plane::{reply, request},
    read_u64, write_u32, write_u64,
};

use crate::KernelPlane;
use crate::arena::{Arena, pages};
use crate::calls::Calls;
use crate::conformance::{self, Fixtures, Refusal};
use crate::model::{Model, ModelHarness, Slot, task_end, task_get, task_put};
use crate::port::{BatchId, CallId, DataPlane, Event, PlaneError, Shape, TaskId};
use crate::verify::verify;

/// The framing `encode_framing` writes for `x: Int64` holding `0, 1, 2, 3`: the schema
/// message and the record batch message, 272 bytes, padded to a page by `record`.
const FRAMING: [u8; 272] = [
    0xff, 0xff, 0xff, 0xff, 0x78, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00,
    0x0c, 0x00, 0x0a, 0x00, 0x09, 0x00, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x04, 0x00, 0x08, 0x00, 0x08, 0x00, 0x00, 0x00, 0x04, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x10, 0x00, 0x14, 0x00,
    0x10, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x04, 0x00, 0x00, 0x00, 0x08, 0x00, 0x10, 0x00, 0x00, 0x00,
    0x18, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x1c, 0x00, 0x00, 0x00,
    0x08, 0x00, 0x0c, 0x00, 0x04, 0x00, 0x0b, 0x00, 0x08, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0xff, 0xb8, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x1a, 0x00,
    0x18, 0x00, 0x17, 0x00, 0x04, 0x00, 0x08, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00,
    0xe0, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
    0x04, 0x00, 0x0a, 0x00, 0x18, 0x00, 0x0c, 0x00, 0x08, 0x00, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00,
    0x2c, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
    0xc0, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xc0, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The record: its framing, padded to a page, then the four values.
fn record() -> Vec<u8> {
    let mut r = vec![0u8; 4096 + 32];
    r[..FRAMING.len()].copy_from_slice(&FRAMING);
    for i in 0..4u64 {
        let at = 4096 + 8 * i as usize;
        r[at..at + 8].copy_from_slice(&i.to_le_bytes());
    }
    r
}

const DEPTH: usize = moruna_abi::limits::SLOT_DEPTH;

fn plane(budget: u64) -> KernelPlane<Model> {
    KernelPlane::new(Model::default(), budget)
}

fn events(p: &mut KernelPlane<Model>) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(e) = p.next(0).expect("next") {
        out.push(e);
    }
    out
}

fn shape(inputs: u32, outputs: u32, port: bool) -> Shape {
    Shape {
        inputs,
        outputs,
        port,
    }
}

/// A producer with a window, its record put and checked: the batch.
fn produced(p: &mut KernelPlane<Model>) -> (TaskId, Slot, BatchId) {
    let (task, _) = p.open(shape(0, 1, false)).expect("open");
    let slot = p.slots(task).expect("slots")[0].clone();
    p.grant(task, 0, 8192).expect("grant");
    assert!(task_get(&slot).is_some());
    task_put(&slot, &record(), record().len() as u64);
    let got = events(p);
    let batch = match got.as_slice() {
        [
            Event::Put {
                task: t,
                output: 0,
                batch,
                rows: 4,
                bytes,
            },
        ] if *t == task && *bytes == record().len() as u64 => *batch,
        other => panic!("{other:?}"),
    };
    (task, slot, batch)
}

#[test]
fn dp_t1_a_put_record_is_checked_and_becomes_a_batch() {
    let mut p = plane(1 << 20);
    let (task, slot, batch) = produced(&mut p);
    let r = p.residency();
    assert_eq!(
        (r.batches, r.resident, r.windows, r.made),
        (1, 4128, 0, 8192)
    );
    let (m, offset, len) = p.batch(batch).expect("batch");
    assert_eq!((offset, len), (0, 4128));
    assert_eq!(&m.bytes.borrow()[4096..4104], &0u64.to_le_bytes());
    task_end(&slot);
    assert_eq!(events(&mut p), vec![Event::Ended { task }]);
    p.drop_batch(batch).expect("drop");
    assert_eq!(events(&mut p), vec![Event::Freed { batch }]);
    let r = p.residency();
    assert_eq!((r.batches, r.pooled), (0, 8192));
    assert_eq!(
        p.drop_batch(batch),
        Err(PlaneError::Unknown("batch")),
        "a freed batch is gone"
    );
}

#[test]
fn dp_t2_fan_out_is_shares_of_one_batch_freed_once_after_the_last() {
    let mut p = plane(1 << 20);
    let (_, _, batch) = produced(&mut p);
    let mut consumers = Vec::new();
    for _ in 0..3 {
        let (task, _) = p.open(shape(1, 0, false)).expect("open");
        let slot = p.slots(task).expect("slots")[0].clone();
        p.offer(task, 0, batch).expect("offer");
        p.finish(task, 0).expect("finish");
        consumers.push((task, slot));
    }
    assert_eq!(p.residency().shares, 3);
    let (held, _, _) = p.batch(batch).expect("batch");
    let held = held.bytes.clone();
    for (_, slot) in &consumers {
        let s = slot.borrow();
        let (m, _, _, _) = s.ready.front().expect("offered");
        assert!(!m.writable, "a consumer's share is read only");
        assert!(Rc::ptr_eq(&m.bytes, &held), "every share is the same bytes");
    }
    p.drop_batch(batch).expect("the engine lets go");
    assert!(events(&mut p).is_empty(), "the shares keep it");
    let mut seen = Vec::new();
    for (i, (task, slot)) in consumers.iter().enumerate() {
        assert_eq!(task_get(slot), Some(record()));
        assert_eq!(task_get(slot), None);
        task_end(slot);
        seen.extend(events(&mut p));
        let freed = seen
            .iter()
            .filter(|e| matches!(e, Event::Freed { .. }))
            .count();
        assert_eq!(freed, usize::from(i == 2), "freed once, after the last");
        assert!(seen.contains(&Event::Released {
            task: *task,
            input: 0,
            batch
        }));
    }
    assert_eq!(p.residency().shares, 0);
    assert_eq!(p.drop_batch(batch), Err(PlaneError::Unknown("batch")));
}

#[test]
fn dp_t3_a_record_that_fails_the_check_ends_the_task_and_is_never_a_batch() {
    let mut p = plane(1 << 20);
    let (task, _) = p.open(shape(0, 1, false)).expect("open");
    let slot = p.slots(task).expect("slots")[0].clone();
    // The values buffer's entry moved past the record: offset 0x0ec0 becomes 0x7ec0.
    let mut outside = record();
    outside[257] = 0x7e;
    for (bytes, len, says) in [
        (vec![0x5au8; 4096], 4096u64, "continuation marker"),
        (outside, 4128, "outside the body"),
        (record(), 4096 + 8, "the record holds"),
        (record()[..200].to_vec(), 200, "past the 200 bytes put"),
    ] {
        p.grant(task, 0, 8192).expect("grant");
        assert!(task_get(&slot).is_some());
        task_put(&slot, &bytes, len);
        match events(&mut p).as_slice() {
            [
                Event::Refused {
                    task: t,
                    output: 0,
                    reason,
                },
            ] if *t == task => assert!(reason.contains(says), "{reason}"),
            other => panic!("{other:?}"),
        }
        let told = slot
            .borrow_mut()
            .refused
            .take()
            .expect("the task was ended");
        assert!(told.contains(says));
        assert_eq!(p.residency().batches, 0);
    }
    // The refused windows went back to the pool: one object served all four.
    assert_eq!(p.calls().made, 1);
    // A task that ended on its own before the refusal is not an error.
    p.grant(task, 0, 8192).expect("grant");
    assert!(task_get(&slot).is_some());
    task_put(&slot, &[0u8; 64], 64);
    task_end(&slot);
    let got = events(&mut p);
    assert!(
        matches!(got.first(), Some(Event::Refused { .. })),
        "{got:?}"
    );
    assert_eq!(got.last(), Some(&Event::Ended { task }));
}

#[test]
fn dp_t4_port_calls_are_answered_with_a_mapped_reply() {
    let mut p = plane(1 << 20);
    let (task, end) = p.open(shape(0, 0, true)).expect("open");
    let end = end.expect("the kernel's end");
    let mut req = vec![0u8; request::SIZE];
    write_u32(&mut req, request::KIND, request::CALL);
    write_u64(&mut req, request::TOKEN, 77);
    write_u64(&mut req, request::PORT, 5);
    write_u64(&mut req, request::LEN, 3);
    req.extend_from_slice(b"abc");
    let mut fake = Model::default();
    fake.send(&end, &req, None).expect("send");
    let call = match events(&mut p).as_slice() {
        [
            Event::Call {
                call,
                port: 5,
                request,
            },
        ] if request == b"abc" => *call,
        other => panic!("{other:?}"),
    };
    assert_eq!(call, CallId { task, token: 77 });
    p.answer(call, Ok(b"reply")).expect("answer");
    p.answer(call, Ok(&[])).expect("an empty answer");
    p.answer(call, Err(Error::AccessDenied)).expect("a refusal");
    let mut got = end.state.borrow_mut();
    let (bytes, memory) = got.queues[1].pop_front().expect("a reply");
    assert_eq!(read_u64(&bytes, reply::TOKEN), Some(77));
    assert_eq!(read_u64(&bytes, reply::LEN), Some(5));
    let memory = memory.expect("the reply's memory");
    assert!(!memory.writable);
    assert_eq!(&memory.bytes.borrow()[..5], b"reply");
    let (bytes, memory) = got.queues[1].pop_front().expect("a reply");
    assert!(memory.is_none() && read_u64(&bytes, reply::LEN) == Some(0));
    let (bytes, _) = got.queues[1].pop_front().expect("a reply");
    assert_eq!(
        read_u64(&bytes, reply::STATUS),
        Some(Error::AccessDenied.code() as u64)
    );
    got.open[1] = false;
    drop(got);
    assert_eq!(events(&mut p), vec![Event::Ended { task }]);
    assert_eq!(
        p.answer(call, Ok(b"late")),
        Err(PlaneError::Unknown("task"))
    );
}

#[test]
fn dp_t4_a_port_message_that_is_no_request_is_an_error() {
    let mut p = plane(1 << 20);
    let (_, end) = p.open(shape(0, 0, true)).expect("open");
    let end = end.expect("end");
    Model::default().send(&end, &[1, 2, 3], None).expect("send");
    assert!(matches!(p.next(0), Err(PlaneError::Unknown(_))));
}

#[test]
fn dp_t5_the_arena_keeps_its_budget_and_reuses_what_comes_back() {
    let mut p = plane(16384);
    let (task, _) = p.open(shape(0, 1, false)).expect("open");
    let slot = p.slots(task).expect("slots")[0].clone();
    p.grant(task, 0, 8192).expect("grant");
    p.grant(task, 0, 4096).expect("grant");
    assert_eq!(
        p.grant(task, 0, 8192),
        Err(PlaneError::Budget {
            asked: 8192,
            free: 4096
        })
    );
    assert_eq!(p.residency().windows, 12288);
    task_end(&slot);
    assert_eq!(events(&mut p), vec![Event::Ended { task }]);
    let r = p.residency();
    assert_eq!((r.windows, r.pooled, r.made), (0, 12288, 12288));
    let (task, _) = p.open(shape(0, 1, false)).expect("open");
    p.grant(task, 0, 100).expect("the smallest that fits");
    assert_eq!(p.residency().pooled, 8192);
    assert_eq!(p.calls().made, 2);
    assert_eq!(
        p.grant(task, 0, 0),
        Err(PlaneError::Limit("a window of no bytes, or past u64"))
    );
    assert_eq!(p.grant(task, 9, 4096), Err(PlaneError::Unknown("output")));
}

#[test]
fn dp_t6_a_full_or_closed_slot_and_unknown_names() {
    let mut p = plane(1 << 20);
    let (_, _, batch) = produced(&mut p);
    let (task, _) = p.open(shape(1, 0, false)).expect("open");
    for _ in 0..DEPTH {
        p.offer(task, 0, batch).expect("offer");
    }
    assert_eq!(p.offer(task, 0, batch), Err(PlaneError::Full));
    p.calls().fail_push = Some(Error::NoMemory);
    assert_eq!(
        p.offer(task, 0, batch),
        Err(PlaneError::Kernel(Error::NoMemory))
    );
    p.finish(task, 0).expect("finish");
    assert_eq!(p.offer(task, 0, batch), Err(PlaneError::Closed));
    assert_eq!(p.offer(task, 1, batch), Err(PlaneError::Unknown("input")));
    assert_eq!(
        p.offer(TaskId(99), 0, batch),
        Err(PlaneError::Unknown("task"))
    );
    assert_eq!(
        p.offer(task, 0, BatchId(99)),
        Err(PlaneError::Unknown("batch"))
    );
    assert_eq!(p.finish(task, 3), Err(PlaneError::Unknown("input")));
    assert_eq!(p.finish(TaskId(99), 0), Err(PlaneError::Unknown("task")));
    assert!(p.slots(TaskId(99)).is_err());
    assert!(p.batch(BatchId(99)).is_err());
    let (out, _) = p.open(shape(0, 1, false)).expect("open");
    p.calls().fail_push = Some(Error::ShouldWait);
    assert_eq!(p.grant(out, 0, 4096), Err(PlaneError::Full));
    p.calls().fail_push = Some(Error::BadState);
    assert_eq!(p.grant(out, 0, 4096), Err(PlaneError::Closed));
    p.calls().fail_push = Some(Error::NoMemory);
    assert_eq!(
        p.grant(out, 0, 4096),
        Err(PlaneError::Kernel(Error::NoMemory))
    );
    p.calls().fail_memory = true;
    assert!(matches!(
        p.grant(out, 0, 1 << 30),
        Err(PlaneError::Budget { .. })
    ));
}

#[test]
fn dp_t7_bounds_of_a_plane() {
    let mut p = plane(1 << 20);
    assert_eq!(
        p.open(shape(65, 0, false)).err(),
        Some(PlaneError::Limit("slots of one task"))
    );
    p.open(shape(32, 31, true)).expect("63 slots and a port");
    assert_eq!(
        p.open(shape(1, 0, false)).err(),
        Some(PlaneError::Limit("slots and ports waited on at once"))
    );
    let mut empty = plane(1 << 20);
    assert_eq!(empty.next(0), Ok(None), "nothing to wait on");
    let (task, _) = empty.open(shape(1, 0, false)).expect("open");
    assert_eq!(empty.next(0), Ok(None), "the deadline passes");
    let slot = empty.slots(task).expect("slots")[0].clone();
    task_end(&slot);
    assert_eq!(events(&mut empty), vec![Event::Ended { task }]);
}

#[test]
fn dp_t8_the_engine_loads_its_own_records() {
    let mut p = plane(1 << 20);
    let (batch, rows) = p.load(&record()).expect("load");
    assert_eq!(rows, 4);
    assert_eq!(p.residency().batches, 1);
    match p.load(&[7u8; 64]) {
        Err(PlaneError::Refused(reason)) => assert!(reason.contains("continuation")),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.residency().batches, 1);
    p.drop_batch(batch).expect("drop");
    assert_eq!(p.drop_batch(batch), Err(PlaneError::Unknown("batch")));
    p.calls().fail_memory = true;
    let mut tight = KernelPlane::new(
        Model {
            fail_memory: true,
            ..Model::default()
        },
        1 << 20,
    );
    assert_eq!(
        tight.load(&record()).err(),
        Some(PlaneError::Kernel(Error::NoMemory))
    );
}

#[test]
fn kd2_t6_the_engine_writes_a_record_in_place() {
    let mut p = plane(1 << 20);
    let r = record();
    // Written piece by piece into the object, as a stage lays a batch's buffers out.
    let (batch, rows) = p
        .load_with(r.len() as u64, |calls, memory| {
            let (head, body) = r.split_at(4096);
            calls.write(memory, 0, head)?;
            calls.write(memory, 4096, body)
        })
        .expect("load_with");
    assert_eq!(rows, 4);
    let (_, offset, len) = p.batch(batch).expect("batch");
    assert_eq!((offset, len), (0, r.len() as u64));
    assert_eq!(p.residency().batches, 1);
    // A write that fails gives the object back and makes no batch; so does a refusal.
    assert_eq!(
        p.load_with(4096, |_, _| Err(Error::Fault)).err(),
        Some(PlaneError::Kernel(Error::Fault))
    );
    match p.load_with(64, |calls, memory| calls.write(memory, 0, &[7u8; 64])) {
        Err(PlaneError::Refused(reason)) => assert!(reason.contains("continuation")),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.residency().batches, 1);
    p.drop_batch(batch).expect("drop");
    assert_eq!(p.residency().batches, 0);
    assert!(
        p.residency().pooled > 0,
        "what came back is pooled for the next"
    );
}

#[test]
fn dp_t9_verify_reads_only_the_framing() {
    let r = record();
    let mut reads = Vec::new();
    let checked = verify(r.len() as u64, |at, buf| {
        reads.push((at, buf.len()));
        buf.copy_from_slice(&r[at as usize..at as usize + buf.len()]);
        Ok(())
    })
    .expect("verify");
    assert_eq!(checked.rows, 4);
    assert_eq!(reads, vec![(0, 4096)], "one page, never the body");
    // A record shorter than a page is read whole, and a framing past it is refused.
    let short = &r[..100];
    let refused = verify(100, |at, buf| {
        buf.copy_from_slice(&short[at as usize..at as usize + buf.len()]);
        Ok(())
    });
    assert!(refused.is_err());
    // A framing that needs more than the first read takes a second.
    let mut long = vec![0u8; 8];
    long[..4].copy_from_slice(&[0xff; 4]);
    long[4..8].copy_from_slice(&5000u32.to_le_bytes());
    long.resize(8000, 0);
    let mut reads = 0;
    let refused = verify(long.len() as u64, |at, buf| {
        reads += 1;
        buf.copy_from_slice(&long[at as usize..at as usize + buf.len()]);
        Ok(())
    });
    assert!(refused.is_err());
    assert_eq!(reads, 2);
    // A framing whose two messages end past the first page is read to its end.
    let mut wide = long.clone();
    wide[5008..5012].copy_from_slice(&[0xff; 4]);
    wide[5012..5016].copy_from_slice(&16u32.to_le_bytes());
    let mut sizes = Vec::new();
    let refused = verify(wide.len() as u64, |at, buf| {
        sizes.push((at, buf.len()));
        buf.copy_from_slice(&wide[at as usize..at as usize + buf.len()]);
        Ok(())
    });
    assert!(refused.is_err());
    assert_eq!(sizes, vec![(0, 4096), (4096, 920), (5016, 16)]);
    // Fewer bytes than a message header.
    let tiny = verify(4, |_, buf| {
        buf.fill(0xff);
        Ok(())
    });
    assert!(tiny.expect_err("too short").contains("inside its framing"));
    let failed = verify(r.len() as u64, |_, _| Err(Error::Fault));
    assert!(
        failed
            .expect_err("unreadable")
            .contains("could not be read")
    );
    let mut p = plane(1 << 20);
    p.calls().fail_read = true;
    let (task, _) = p.open(shape(0, 1, false)).expect("open");
    let slot = p.slots(task).expect("slots")[0].clone();
    p.grant(task, 0, 8192).expect("grant");
    assert!(task_get(&slot).is_some());
    task_put(&slot, &record(), 4128);
    assert!(matches!(events(&mut p).as_slice(), [Event::Refused { .. }]));
}

#[test]
fn dp_t5_arena_alone() {
    let mut a: Arena<u32> = Arena::new(8192);
    assert_eq!(pages(1), Some(4096));
    assert_eq!(pages(u64::MAX), None);
    let (id, h, size) = a.take(10, |_| Ok(5)).expect("take");
    assert_eq!((h, size), (5, 4096));
    assert_eq!(
        a.free(id),
        Err(PlaneError::Unknown("an object a slot still holds"))
    );
    assert_eq!(
        a.free(99),
        Err(PlaneError::Unknown("an object the arena did not make"))
    );
    assert_eq!(
        a.hold(99, 1),
        Err(PlaneError::Unknown("an object the arena did not make"))
    );
    a.hold(id, h).expect("hold");
    assert_eq!(
        a.hold(id, 6),
        Err(PlaneError::Unknown("an object the arena already holds"))
    );
    a.free(id).expect("free");
    a.free(id).expect("freeing twice pools it once");
    assert_eq!(a.pooled(), 4096);
    assert_eq!(a.bytes(99), 0);
    assert!(a.handle(id).is_some());
    let made = a.take(4096, |_| Ok(7)).expect("the pooled one");
    assert_eq!(made.1, 5);
    assert_eq!(
        a.take(4096, |_| Err(Error::NoMemory)).err(),
        Some(PlaneError::Kernel(Error::NoMemory))
    );
    assert_eq!(
        a.take(0, |_| Ok(1)).err(),
        Some(PlaneError::Limit("a window of no bytes, or past u64"))
    );
}

#[test]
fn dp_t5_pooled_objects_too_small_are_given_up_for_a_window_that_fits_the_budget() {
    let mut a: Arena<u32> = Arena::new(16384);
    let (small, h, _) = a.take(4096, |_| Ok(1)).expect("take");
    a.untake(small, h).expect("pooled");
    let (other, h, _) = a.take(4096, |_| Ok(2)).expect("the pooled one");
    assert_eq!(other, small);
    a.untake(other, h).expect("pooled again");
    let (_, _, size) = a.take(8192, |_| Ok(3)).expect("a new one beside the pool");
    assert_eq!(size, 8192);
    assert_eq!((a.made(), a.pooled()), (12288, 4096));
    // 8192 more do not fit beside 12288 made: the pooled page is given up for them.
    let (_, h, _) = a.take(8192, |_| Ok(4)).expect("the pool given up");
    assert_eq!(h, 4);
    assert_eq!((a.made(), a.pooled()), (16384, 0));
    assert_eq!(
        a.take(4096, |_| Ok(5)).err(),
        Some(PlaneError::Budget {
            asked: 4096,
            free: 0
        })
    );
}

#[test]
fn dp_t5_a_retired_object_leaves_the_arena() {
    let mut a: Arena<u32> = Arena::new(8192);
    let (id, h, _) = a.take(4096, |_| Ok(9)).expect("take");
    assert_eq!(
        a.retire(id),
        Err(PlaneError::Unknown("an object a slot still holds"))
    );
    a.hold(id, h).expect("hold");
    a.free(id).expect("pooled");
    assert_eq!(a.retire(id), Ok(9), "the handle comes back to be closed");
    assert_eq!((a.made(), a.pooled()), (0, 0));
    assert_eq!(
        a.retire(id),
        Err(PlaneError::Unknown("an object the arena did not make"))
    );
}

/// The fixtures a host without arrow can make: the record above, and the refusals of DP-T3.
fn fixtures() -> Fixtures {
    let mut outside = record();
    outside[257] = 0x7e;
    let refusal = |label: &str, bytes: Vec<u8>, put: u64, says: &str| Refusal {
        label: String::from(label),
        bytes,
        put,
        says: String::from(says),
    };
    Fixtures {
        good: record(),
        rows: 4,
        refusals: vec![
            refusal("garbage", vec![0x5a; 4096], 4096, "continuation marker"),
            refusal("outside", outside, 4128, "outside the body"),
            refusal("short", record(), 4096 + 8, "the record holds"),
            refusal(
                "framing",
                record()[..200].to_vec(),
                200,
                "past the 200 bytes put",
            ),
        ],
    }
}

#[test]
fn ka2_the_port_conformance_suite_passes_on_the_kernel_model() {
    let mut harness = ModelHarness::default();
    let results = conformance::run_all(&mut harness, &fixtures());
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(results.len(), 13);
}

#[test]
fn ka2_scripts_and_outcomes_cross_as_bytes() {
    use conformance::{Seen, Step, decode_script, decode_seen, encode_script, encode_seen};
    let script = vec![
        Step::Get(3),
        Step::Write {
            slot: 1,
            at: 4096,
            bytes: vec![1, 2, 3],
        },
        Step::Put {
            slot: 1,
            offset: 0,
            len: 9,
        },
        Step::WriteAfterPut(1),
        Step::Call {
            port: 7,
            request: b"abc".to_vec(),
        },
        Step::Crash,
    ];
    assert_eq!(decode_script(&encode_script(&script)), Ok(script));
    let seen = vec![
        Seen::Batch { len: 5, sum: 6 },
        Seen::End,
        Seen::Window { len: 4096 },
        Seen::Wrote,
        Seen::Put,
        Seen::Reply(b"r".to_vec()),
        Seen::Error(Error::Limit),
    ];
    let bytes: Vec<u8> = seen.iter().flat_map(encode_seen).collect();
    assert_eq!(decode_seen(&bytes), Ok(seen));
    assert!(decode_script(&[9]).is_err());
    assert!(decode_script(&[1, 0]).is_err());
    assert!(decode_seen(&[7, 99, 0, 0, 0]).is_err());
    assert!(decode_seen(&[0]).is_err());
}
