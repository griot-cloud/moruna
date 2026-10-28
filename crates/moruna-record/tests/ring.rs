//! The ring's properties (design 21 k, RC-T12 to RC-T22).

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use moruna_record::ring::{HEADER, MAGIC};
use moruna_record::{Decoded, Error, Full, PushError, Ring, decode, encode};

/// Only its size and alignment matter.
#[repr(align(64))]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Line([u8; 64]);

/// A zeroed, 64-byte-aligned block of memory that can be handed out as several `&mut [u8]` over
/// the same bytes, which is what a second process mapping the same shared memory is.
struct Shared {
    _lines: Vec<Line>,
    ptr: *mut u8,
    len: usize,
}

impl Shared {
    fn new(bytes: usize) -> Shared {
        let mut lines = vec![Line([0; 64]); bytes.div_ceil(64)];
        let ptr = lines.as_mut_ptr().cast::<u8>();
        Shared {
            len: lines.len() * 64,
            _lines: lines,
            ptr,
        }
    }

    /// A ring region with a data area of `capacity` bytes.
    fn ring(capacity: usize) -> Shared {
        Shared::new(HEADER + capacity)
    }

    /// The whole block as a region. Each call is another mapping of the same memory.
    #[allow(clippy::mut_from_ref)]
    fn region(&self) -> &mut [u8] {
        // SAFETY: `ptr` and `len` describe the live allocation `_lines` owns. The slices this
        // returns alias, as two processes' mappings of one segment do; a test touches the bytes
        // through them only while no other thread does, or through the ring's atomics.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    fn put_u64(&self, at: usize, v: u64) {
        self.region()[at..at + 8].copy_from_slice(&v.to_ne_bytes());
    }

    fn put_u32(&self, at: usize, v: u32) {
        self.region()[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn get_u64(&self, at: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.region()[at..at + 8]);
        u64::from_ne_bytes(b)
    }
}

const OFF_CAPACITY: usize = 8;
const OFF_RESERVE: usize = 64;
const OFF_COMMIT: usize = 128;
const OFF_READ: usize = 192;
const OFF_DROPPED: usize = 256;

fn rec(seq: u64, body_len: usize) -> Vec<u8> {
    encode(
        seq,
        seq as i64 * 10,
        5,
        &format!("id-{seq}"),
        &[("h", "v")],
        &vec![seq as u8; body_len],
    )
    .expect("fits")
}

fn seq_of(bytes: &[u8]) -> u64 {
    match decode(bytes) {
        Decoded::Record(r, n) => {
            assert_eq!(n, bytes.len());
            r.seq
        }
        other => panic!("not a record: {other:?}"),
    }
}

#[test]
fn rc_t12_records_come_out_in_the_order_they_went_in() {
    let mem = Shared::ring(4096);
    let ring = Ring::init(mem.region()).expect("init");
    assert_eq!(ring.capacity(), 4096);
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    assert!(c.pop().is_none(), "an empty ring");
    for seq in 0..20 {
        p.push(&rec(seq, (seq as usize * 7) % 50)).expect("room");
    }
    for seq in 0..20 {
        let bytes = c.pop().expect("a record");
        assert_eq!(seq_of(bytes), seq);
        assert_eq!(bytes, &rec(seq, (seq as usize * 7) % 50)[..]);
    }
    assert!(c.pop().is_none());
    // push_record encodes in place and produces the same bytes as encode.
    p.push_record(77, 770, 5, "id-77", &[("h", "v")], &[77u8; 3])
        .expect("room");
    assert_eq!(c.pop(), Some(&rec(77, 3)[..]));
    // An empty slice is a slot of its own.
    p.push(&[]).expect("room");
    assert_eq!(c.pop(), Some(&[][..]));
    assert!(c.pop().is_none());
}

#[test]
fn rc_t13_a_slot_that_would_cross_the_end_is_padded_and_skipped() {
    let mem = Shared::ring(128);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    // Empty slots of 8 bytes walk the write position to wherever a test wants it.
    let advance_to = |c: &mut moruna_record::Consumer<'_>, at: u64| {
        while ring.reserved() % 128 != at {
            p.push(&[]).expect("room");
            assert_eq!(c.pop(), Some(&[][..]));
            assert!(c.pop().is_none());
        }
    };
    // A slot of 16 with 8 left, a slot of 24 with 16 left: the pad is 8 and 16 bytes, and is
    // skipped; the record after it comes out.
    for (at, len, pad) in [
        (120u64, 8usize, 8u64),
        (112, 16, 16),
        (8, 40, 0),
        (96, 48, 32),
    ] {
        advance_to(&mut c, at);
        let before = ring.reserved();
        let bytes = vec![len as u8; len];
        p.push(&bytes).expect("room");
        let slot = (8 + len as u64).div_ceil(8) * 8;
        assert_eq!(
            ring.reserved() - before,
            pad + slot,
            "pad {pad} then a slot of {slot}"
        );
        assert_eq!(c.pop(), Some(&bytes[..]));
        assert!(c.pop().is_none());
        assert_eq!(
            ring.released(),
            ring.reserved(),
            "the padding was released too"
        );
    }
    // Several records after one padding, in order.
    advance_to(&mut c, 120);
    let (first, second, third) = (vec![1u8; 8], vec![2u8; 8], vec![3u8; 16]);
    p.push(&first).expect("room");
    p.push(&second).expect("room");
    p.push(&third).expect("room");
    assert_eq!(c.pop(), Some(&first[..]));
    assert_eq!(c.pop(), Some(&second[..]));
    assert_eq!(c.pop(), Some(&third[..]));
    assert!(c.pop().is_none());
    assert!(
        ring.reserved() > 3 * 128,
        "the ring went round more than once"
    );
    assert_eq!(ring.dropped(), 0);
}

#[test]
fn rc_t14_a_full_ring_refuses_counts_and_never_waits() {
    let mem = Shared::ring(64);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    let a = [1u8; 24];
    p.push(&a).expect("room");
    p.push(&[2u8; 24]).expect("exactly the capacity is room");
    assert_eq!(ring.reserved(), 64);
    assert_eq!(p.push(&[3u8; 24]), Err(Full));
    assert_eq!(p.push(&[]), Err(Full));
    assert_eq!(ring.dropped(), 2);
    assert_eq!(c.dropped(), 2);
    // A record that could never fit is refused the same way.
    assert_eq!(p.push(&[0u8; 100]), Err(Full));
    assert_eq!(ring.dropped(), 3);
    assert_eq!(c.pop(), Some(&a[..]));
    assert_eq!(
        p.push(&[4u8; 24]),
        Err(Full),
        "the record popped is not released yet"
    );
    assert_eq!(c.pop(), Some(&[2u8; 24][..]));
    p.push(&[5u8; 24]).expect("the first slot is released");
    assert_eq!(c.pop(), Some(&[5u8; 24][..]));
    assert!(c.pop().is_none());
    assert_eq!(ring.dropped(), 4);

    // A padding that cannot be claimed refuses the record, too: reserve 112, read 48.
    let mem = Shared::ring(64);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    for i in 0..4u8 {
        p.push(&[i; 8]).expect("room");
    }
    for i in 0..3u8 {
        assert_eq!(c.pop(), Some(&[i; 8][..]));
        if i > 0 {
            p.push(&[10 + i; 8]).expect("room");
        }
    }
    assert!(c.pop().is_some());
    p.push(&[13; 8]).expect("room");
    assert_eq!((ring.reserved(), ring.released()), (112, 48));
    assert_eq!(p.push(&[0u8; 16]), Err(Full));
    assert_eq!(ring.dropped(), 1);
    assert_eq!(ring.reserved(), 112, "nothing was claimed");

    // A record that fits the ring but not now, once its padding is claimed.
    let mem = Shared::ring(64);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    p.push(&[1u8; 16]).expect("room");
    p.push(&[2u8; 16]).expect("room");
    assert_eq!(p.push(&[3u8; 24]), Err(Full));
    assert_eq!(ring.reserved(), 64, "the padding to the end stayed");
    let mut c = ring.consumer().expect("consumer");
    assert_eq!(c.pop(), Some(&[1u8; 16][..]));
    assert_eq!(c.pop(), Some(&[2u8; 16][..]));
    assert!(c.pop().is_none(), "the padding is skipped, not returned");
    assert_eq!(ring.released(), 64);
}

/// Split `n` into (thread, n) from an id `"<thread>-<n>"`.
fn parse_id(id: &str) -> (usize, usize) {
    let (t, n) = id.split_once('-').expect("an id of two numbers");
    (t.parse().expect("thread"), n.parse().expect("n"))
}

#[test]
fn rc_t15_eight_producers_and_a_consumer_lose_and_repeat_nothing() {
    const THREADS: usize = 8;
    const EACH: usize = 200_000;
    let mem = Shared::ring(1 << 20);
    let ring = Ring::init(mem.region()).expect("init");
    assert_eq!(ring.capacity(), 1 << 20);
    let finished = AtomicUsize::new(0);

    let (accepted, arrived) = std::thread::scope(|s| {
        let producers: Vec<_> = (0..THREADS)
            .map(|t| {
                let p = ring.producer();
                let finished = &finished;
                s.spawn(move || {
                    let mut accepted = vec![false; EACH];
                    let mut refused = 0u64;
                    for (n, slot) in accepted.iter_mut().enumerate() {
                        let id = format!("{t}-{n}");
                        let mut body = (n as u32).to_le_bytes().to_vec();
                        body.extend(std::iter::repeat_n(t as u8, n % 7));
                        match p.push_record(n as u64, (n * 3) as i64, t as u32, &id, &[], &body) {
                            Ok(()) => *slot = true,
                            Err(PushError::Full) => {
                                refused += 1;
                                // Let the consumer catch up; the record is not retried.
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("{e}"),
                        }
                    }
                    finished.fetch_add(1, Ordering::Release);
                    (accepted, refused)
                })
            })
            .collect();

        let mut c = ring.consumer().expect("consumer");
        let mut arrived = vec![vec![false; EACH]; THREADS];
        let mut last: [Option<usize>; THREADS] = [None; THREADS];
        loop {
            let done = finished.load(Ordering::Acquire) == THREADS;
            match c.pop() {
                Some(bytes) => {
                    let Decoded::Record(r, n) = decode(bytes) else {
                        panic!("an arrival that is not a record");
                    };
                    assert_eq!(n, bytes.len());
                    let (t, i) = parse_id(r.msg_id);
                    assert_eq!(
                        (r.seq, r.ts, r.subject),
                        (i as u64, (i * 3) as i64, t as u32)
                    );
                    assert_eq!(r.body[..4], (i as u32).to_le_bytes());
                    assert!(r.body[4..].iter().all(|&b| b == t as u8) && r.body.len() == 4 + i % 7);
                    assert!(
                        last[t].is_none_or(|l| l < i),
                        "thread {t}: {i} after {:?}",
                        last[t]
                    );
                    last[t] = Some(i);
                    assert!(!arrived[t][i], "({t}, {i}) arrived twice");
                    arrived[t][i] = true;
                }
                None if done => break,
                None => std::thread::yield_now(),
            }
        }
        let accepted: Vec<_> = producers
            .into_iter()
            .map(|h| h.join().expect("a producer"))
            .collect();
        (accepted, arrived)
    });

    let mut pushed_ok = 0usize;
    let mut refused_total = 0u64;
    for (t, (accepted, refused)) in accepted.iter().enumerate() {
        assert_eq!(
            *accepted, arrived[t],
            "thread {t}: what was accepted is what arrived"
        );
        pushed_ok += accepted.iter().filter(|&&a| a).count();
        refused_total += refused;
    }
    let arrived_total: usize = arrived.iter().flatten().filter(|&&a| a).count();
    assert_eq!(arrived_total, pushed_ok);
    eprintln!(
        "rc_t15: {arrived_total} arrived, {} refused",
        ring.dropped()
    );
    assert!(arrived_total >= 10_000, "only {arrived_total} arrived");
    assert_eq!(
        ring.dropped(),
        refused_total,
        "every refusal is counted, exactly"
    );
    assert_eq!(
        arrived_total as u64 + ring.dropped(),
        (THREADS * EACH) as u64
    );
    assert_eq!(ring.reserved(), ring.committed());
}

#[test]
fn rc_t16_a_second_handle_on_the_same_region_sees_what_the_first_pushed() {
    let mem = Shared::ring(4096);
    let first = Ring::init(mem.region()).expect("init");
    let p = first.producer();
    for seq in 0..10 {
        p.push(&rec(seq, 5)).expect("room");
    }
    // The second process maps the same memory and attaches.
    let second = Ring::attach(mem.region()).expect("attach");
    assert_eq!(second.capacity(), first.capacity());
    assert_eq!(second.reserved(), first.reserved());
    let mut c = second.consumer().expect("consumer");
    for seq in 0..5 {
        assert_eq!(seq_of(c.pop().expect("a record")), seq);
    }
    // The first handle keeps pushing; the second pops it.
    p.push(&rec(10, 5)).expect("room");
    second.producer().push(&rec(11, 5)).expect("room");
    for seq in 5..12 {
        assert_eq!(seq_of(c.pop().expect("a record")), seq);
    }
    assert!(c.pop().is_none());
    drop(c);
    assert_eq!(first.released(), second.released());
    assert_eq!(
        first.released(),
        first.reserved(),
        "the held slot is released by the next pop"
    );
    // A consumer that attaches after everyone left resumes where the last released.
    let again = Ring::attach(mem.region()).expect("attach");
    assert!(again.consumer().expect("consumer").pop().is_none());
}

#[test]
fn rc_t17_a_region_that_is_not_a_ring_is_refused() {
    // Garbage.
    let mem = Shared::ring(4096);
    for (i, b) in mem.region().iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    assert!(matches!(Ring::attach(mem.region()), Err(Error::BadMagic)));
    // Zeros: never formatted.
    let mem = Shared::ring(4096);
    assert!(matches!(Ring::attach(mem.region()), Err(Error::BadMagic)));
    // One bit of the magic.
    Ring::init(mem.region()).expect("init");
    mem.put_u64(0, MAGIC ^ 1);
    assert!(matches!(Ring::attach(mem.region()), Err(Error::BadMagic)));
    mem.put_u64(0, MAGIC);
    assert!(Ring::attach(mem.region()).is_ok());
    // A header that lies.
    for (capacity, why) in [
        (1000u64, "power of two"),
        (32, "outside"),
        (1 << 31, "outside"),
        (8192, "larger than the region"),
    ] {
        mem.put_u64(OFF_CAPACITY, capacity);
        let Err(Error::BadHeader(found)) = Ring::attach(mem.region()) else {
            panic!("capacity {capacity} was taken");
        };
        assert!(found.contains(why), "{found}");
    }
    mem.put_u64(OFF_CAPACITY, 4096);
    mem.put_u64(OFF_READ, 8);
    assert!(
        matches!(Ring::attach(mem.region()), Err(Error::BadHeader(_))),
        "read past reserve"
    );
    mem.put_u64(OFF_READ, 0);
    mem.put_u64(OFF_RESERVE, 4097);
    assert!(
        matches!(Ring::attach(mem.region()), Err(Error::BadHeader(_))),
        "more than a lap ahead"
    );
    // Shape: alignment and size, for both entry points.
    let block = Shared::new(HEADER + 4096 + 64);
    let off = &mut block.region()[8..8 + HEADER + 4096];
    assert!(matches!(Ring::init(off), Err(Error::Misaligned)));
    let off = &mut block.region()[8..8 + HEADER + 4096];
    assert!(matches!(Ring::attach(off), Err(Error::Misaligned)));
    let small = Shared::new(HEADER + 32);
    let want = HEADER + 64;
    assert!(matches!(
        Ring::init(&mut small.region()[..HEADER + 63]),
        Err(Error::RegionTooSmall { need, .. }) if need == want
    ));
    assert!(matches!(
        Ring::attach(&mut small.region()[..HEADER]),
        Err(Error::RegionTooSmall { .. })
    ));
    // Formatting rounds the data area down to a power of two and clears what was there.
    let mem = Shared::new(HEADER + 1024);
    mem.region().fill(0xEE);
    let ring = Ring::init(&mut mem.region()[..HEADER + 1000]).expect("init");
    assert_eq!(ring.capacity(), 512);
    assert!(ring.consumer().expect("consumer").pop().is_none());
    assert_eq!(
        (
            ring.reserved(),
            ring.committed(),
            ring.released(),
            ring.dropped()
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn rc_t18_a_dead_producers_slot_is_skipped_when_the_caller_says_it_is_stale() {
    let mem = Shared::ring(4096);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    let a = rec(1, 10);
    p.push(&a).expect("room");
    // A producer claims 48 bytes at the next position, writes its length word, and dies.
    let dead = ring.reserved();
    let slot = 48u64;
    mem.put_u64(OFF_RESERVE, dead + slot);
    mem.put_u32(HEADER + dead as usize, 40 | (1 << 31));
    // Records published after it are held back: order is kept.
    let b = rec(3, 4);
    p.push(&b).expect("room");
    assert_eq!(c.pop(), Some(&a[..]));
    assert!(c.pop().is_none(), "the slot being written is not ready");
    assert!(
        c.pop_or_skip_stale(|_| false).is_none(),
        "and a caller that says it is live waits"
    );

    let asked = RefCell::new(Vec::new());
    let got = c.pop_or_skip_stale(|pos| {
        asked.borrow_mut().push(pos);
        pos == dead
    });
    assert_eq!(got, Some(&b[..]));
    assert_eq!(
        *asked.borrow(),
        vec![dead],
        "asked once, about the slot that was claimed"
    );
    assert!(c.pop().is_none());
    assert!(c.fault().is_none());
    let committed = ring.committed();
    assert_eq!(committed, ring.reserved(), "the dead slot is settled");

    // An empty ring is never asked about.
    assert!(
        c.pop_or_skip_stale(|_| panic!("nothing was claimed"))
            .is_none()
    );

    // A producer that died before it wrote its length: no one knows where its slot ends.
    let mem = Shared::ring(4096);
    let ring = Ring::init(mem.region()).expect("init");
    let mut c = ring.consumer().expect("consumer");
    mem.put_u64(OFF_RESERVE, 48);
    assert!(c.pop_or_skip_stale(|_| true).is_none());
    assert_eq!(c.fault(), Some(Error::StaleExtentUnknown { pos: 0 }));
    ring.producer().push(&[1, 2, 3]).expect("room");
    assert!(
        c.pop_or_skip_stale(|_| true).is_none(),
        "a faulted consumer stays stopped"
    );
    assert!(c.pop().is_none());
}

type Damage = fn(&Shared, &Ring<'_>);

#[test]
fn rc_t19_a_slot_no_producer_wrote_stops_the_consumer_loudly() {
    let cases: [(&str, Damage); 5] = [
        ("a state no one writes", |mem, _| mem.put_u32(HEADER + 4, 7)),
        ("a ready slot never claimed", |mem, _| {
            mem.put_u32(HEADER, 16)
        }),
        ("a length that leaves the data area", |mem, _| {
            mem.put_u32(HEADER, (1 << 30) | (1 << 31))
        }),
        ("a pad that leaves the data area", |mem, ring| {
            mem.put_u32(HEADER, 4096 | (1 << 31));
            mem.put_u32(HEADER + 4, 2);
            let _ = ring;
        }),
        ("a stale slot that leaves the data area", |mem, _| {
            mem.put_u32(HEADER, 5000 | (1 << 31))
        }),
    ];
    for (name, damage) in cases {
        let mem = Shared::ring(4096);
        let ring = Ring::init(mem.region()).expect("init");
        let mut c = ring.consumer().expect("consumer");
        ring.producer().push(&[7u8; 20]).expect("room");
        damage(&mem, &ring);
        if name.starts_with("a ready") {
            mem.put_u32(HEADER + 4, 1);
            mem.put_u32(HEADER, 16);
        }
        let got = c.pop_or_skip_stale(|_| true);
        assert!(got.is_none(), "{name}");
        assert_eq!(c.fault(), Some(Error::CorruptSlot { pos: 0 }), "{name}");
        assert!(c.pop().is_none(), "{name}: stopped for good");
        assert_eq!(c.position(), 0);
    }
}

#[test]
fn rc_t20_one_consumer_per_handle_and_a_dropped_one_gives_back_what_it_held() {
    let mem = Shared::ring(4096);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    p.push(&rec(1, 1)).expect("room");
    p.push(&rec(2, 1)).expect("room");
    let mut c = ring.consumer().expect("consumer");
    assert!(matches!(ring.consumer(), Err(Error::ConsumerTaken)));
    assert_eq!(seq_of(c.pop().expect("one")), 1);
    drop(c);
    let mut c = ring.consumer().expect("the first is gone");
    assert_eq!(
        seq_of(c.pop().expect("one again")),
        1,
        "a held slot is delivered again"
    );
    assert_eq!(seq_of(c.pop().expect("two")), 2);
    assert_eq!(c.position(), ring.released());
}

#[test]
fn rc_t21_push_record_encodes_in_place_and_refuses_what_is_not_a_record() {
    let mem = Shared::ring(256);
    let ring = Ring::init(mem.region()).expect("init");
    let p = ring.producer();
    let big = vec![0u8; moruna_record::MAX_BODY + 1];
    assert_eq!(
        p.push_record(1, 1, 1, "", &[], &big),
        Err(PushError::Record(Error::BodyTooLarge { len: big.len() }))
    );
    assert_eq!(ring.reserved(), 0, "a refused record claims nothing");
    assert_eq!(ring.dropped(), 0);
    let fits_the_format = vec![0u8; 1000];
    assert_eq!(
        p.push_record(1, 1, 1, "", &[], &fits_the_format),
        Err(PushError::Full)
    );
    assert_eq!(ring.dropped(), 1);
    assert!(p.push_record(1, 1, 1, "", &[], b"x").is_ok());

    fn is_error<E: std::error::Error>(_: &E) {}
    is_error(&Full);
    is_error(&PushError::Full);
    assert!(!Full.to_string().is_empty());
    assert!(!PushError::Full.to_string().is_empty());
    assert!(
        PushError::Record(Error::BadMagic)
            .to_string()
            .contains("magic")
    );
}

#[test]
fn rc_t22_the_header_is_the_documented_layout() {
    let mem = Shared::ring(1024);
    let ring = Ring::init(mem.region()).expect("init");
    assert_eq!(HEADER, 320);
    assert_eq!(mem.get_u64(0), MAGIC);
    assert_eq!(MAGIC, u64::from_be_bytes(*b"MRNGREC1"));
    assert_eq!(mem.get_u64(OFF_CAPACITY), 1024);
    let p = ring.producer();
    let mut c = ring.consumer().expect("consumer");
    let a = rec(1, 8);
    let b = rec(2, 8);
    p.push(&a).expect("room");
    p.push(&b).expect("room");
    let slot = |len: usize| (8 + len).div_ceil(8) as u64 * 8;
    assert_eq!(mem.get_u64(OFF_RESERVE), slot(a.len()) + slot(b.len()));
    assert_eq!(
        mem.get_u64(OFF_COMMIT),
        0,
        "nothing settled until the consumer sees it"
    );
    assert_eq!(mem.get_u64(OFF_READ), 0);
    assert!(c.pop().is_some());
    assert_eq!(mem.get_u64(OFF_COMMIT), slot(a.len()));
    assert_eq!(mem.get_u64(OFF_READ), 0, "released by the next pop");
    assert!(c.pop().is_some());
    assert_eq!(mem.get_u64(OFF_READ), slot(a.len()));
    assert_eq!(mem.get_u64(OFF_COMMIT), slot(a.len()) + slot(b.len()));
    assert_eq!(p.push(&[0u8; 2000]), Err(Full));
    assert_eq!(mem.get_u64(OFF_DROPPED), 1);
    assert!(ring.released() <= ring.committed() && ring.committed() <= ring.reserved());
    // A released slot is zero again, head and payload.
    assert!(c.pop().is_none());
    assert!(
        mem.region()[HEADER..HEADER + (ring.released() as usize)]
            .iter()
            .all(|&b| b == 0)
    );
}
