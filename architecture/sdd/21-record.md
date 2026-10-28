# Moruna SDD 21: The record and its ring (`moruna-record`)

**Document type:** software design document, component 21
**Status:** DRAFT · 2026-10-28 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `BOARD.md` item KB4; `architecture/moruna-hosted-engine.md` (the node's logs, events and metrics leave a run as records)
**Preamble:** `00-preamble.md`; **Contracts:** none: the crate depends on nothing
**Component location:** `crates/moruna-record/` (`src/record.rs`, `src/crc.rs`, `src/error.rs`, `src/ring.rs`, `tests/`)
**Consumes:** nothing. **Consumed by:** the node's log, event and metric paths (later items), which push records into a ring on one side and read them out on the other.

**Decisions worth your eye:** (1) the record layout is not Moruna's to choose: it is byte for byte the record format of the event bus the node's records are delivered to, duplicated here so that a record Moruna writes reaches that bus unchanged, and `tests/reference.rs` freezes that format as a copy of its encoder and decoder; (2) `subject` is an opaque `u32` that this crate gives no meaning to; (3) the ring never blocks a producer: a full ring refuses the record and counts it; (4) the ring holds offsets, never pointers, so a region may be shared memory mapped into several processes; (5) a producer that dies after claiming a slot is the caller's to declare dead (`pop_or_skip_stale`), and the one case that cannot be skipped, a producer that dies before it has written the slot's length, stops the consumer loudly instead of guessing (e.5).

---

## a. Purpose and boundary

A run's logs, events and metrics have to leave the VM as records in one format, from several threads and several processes at once, without any of them waiting for the reader and without an allocation on the writing side. This component is that format and that queue.

It owns: the record layout, its encoder and decoder, the crc32c they use, the limits a record must fit, and the ring, a multi-producer, single-consumer queue of encoded records in a byte region the caller provides.

It refuses to know: what a subject means, what a body holds, who maps the region or how, which process is the consumer, and when a producer is dead.

### a.1 What the crate is

`#![no_std]` with `alloc`; the `std` feature (on by default) only adds `std::error::Error` impls. No dependency: the crc is a table generated at compile time. `unsafe` is confined to `ring.rs` and used only to read the region's words as atomics and to copy bytes into or out of a slot a handle owns by the protocol of d.

### a.2 The crc

crc32c (Castagnoli): the reflected polynomial `0x82F63B78`, initial value all ones, final xor all ones, one 256-entry table built by a `const fn`. `crc32c(b"123456789") == 0xE3069283`.

## b. Vocabulary

**Record.** One message in the layout of e.1. **Encoded record.** Its bytes.

**Limits.** `FIXED = 36`, `MAX_BODY = 1 << 20`, `MAX_RECORD = MAX_BODY + 64 * 1024`.

**Region.** The bytes a ring lives in: a header and a data area. **Capacity.** The data area's size, a power of two. **Position.** A `u64` byte counter that only grows; the offset into the data area is `pos & (capacity - 1)`.

**Slot.** A `u32` length word, a `u32` state word and the payload, rounded up to a multiple of 8 bytes. **Claim.** Moving `reserve` past a slot's bytes so that they are the claimer's alone. **Publish.** Storing the slot's state with `Release`. **Release** (the consumer's): zeroing a slot and moving `read` past it.

**Padding.** A slot of state 2 with no record: it fills the data area to its end when the next slot would cross it, or stands in for a slot whose write failed or whose producer died.

### b.1 Decode outcomes

`decode(buf)` returns one of three:

- `Record(record, n)`: a whole record, and the `n` bytes it takes;
- `Incomplete`: the buffer holds fewer bytes than the record says it has (read more, or a torn tail);
- `Corrupt(reason)`: not a record; the reason is one of the `Corruption` variants of e.2.

### b.2 What fits

`check_fits(msg_id, headers, body)` refuses, in this order: a body over `MAX_BODY`; a msg_id over 65535 bytes; over 65535 headers; a header whose key or value is over 65535 bytes; a record whose size, `FIXED` plus the id plus `4 + key + value` for every header plus the body, is over `MAX_RECORD`. `encoded_len` and every encoder apply it first.

## c. Invariants

**Record**

**RC-I1. The layout is e.1's, little-endian, and `len` counts the bytes after itself.** `crc` is the crc32c of bytes `[8, 4 + len)`: everything after it.

**RC-I2. Byte identity with the bus.** For every record that fits (b.2), `encode` returns the bytes the reference encoder returns; and for every input of any length, `decode` returns the outcome the reference decoder returns, with the same reason text for a corrupt one. The reference is frozen in `tests/reference.rs`.

**RC-I3. `decode` returns a record only for bytes that are one.** On any input it neither panics nor reads past the buffer; it returns `Record` only when the length is in `FIXED - 4 ..= MAX_RECORD`, the buffer holds `4 + len` bytes, the crc matches, the id and every header are inside the record and UTF-8, and the body ends where the record does. A single flipped byte is therefore never a different whole record: after the length field it is `Corrupt`, and in the length field it is `Corrupt` or, when the length grew past the buffer, `Incomplete`.

**RC-I4. A refusal writes nothing.** `encode_into` checks the limits and the buffer's size before it writes a byte; a buffer larger than the record keeps its tail untouched.

**RC-I5. The subject is opaque.** No value of `subject` is special to this crate.

**RC-I6. The producing path allocates nothing.** `encode_into`, `Producer::push` and `Producer::push_record` do not allocate.

**Ring**

**RC-I7. The region holds offsets and counters only.** Its header is e.3 and starts on a 64-byte boundary (`init` and `attach` refuse another); each counter is on a line of its own; the data area follows at byte 320.

**RC-I8. Every byte outside a claimed, unreleased slot is zero.** `init` zeroes the region; the consumer zeroes a slot, head and payload, when it releases it. A freshly claimed slot therefore reads state 0, whatever the lap and whatever sizes the earlier laps' slots had.

**RC-I9. A slot never crosses the end of the data area.** A slot that would is preceded by a padding slot that ends at the end.

**RC-I10. A push claims only what fits, and never waits for the consumer.** It claims only when `reserve + slot - read <= capacity`; otherwise it adds one to `dropped`, returns `Full`, and returns at once. The only loop in a push repeats when another producer won the compare-exchange.

**RC-I11. Publication is an acquire and a release.** A producer writes a slot's bytes, then stores state 1 (or 2) with `Release`; the consumer loads the state with `Acquire` before it reads the bytes; the consumer stores `read` with `Release` after it zeroes a slot, and a producer loads `read` with `Acquire` before it claims over it.

**RC-I12. Order.** The consumer returns slots in claim order. A slot still being written holds back every later slot, ready or not.

**RC-I13. Exactly once, in per-producer order.** Every record whose push returned `Ok` is returned by the consumer exactly once, and the records one producer pushed come out in the order it pushed them. Every push that returned `Full` is counted in `dropped`, and nothing else is: arrived plus dropped equals pushed.

**RC-I14. `read <= commit <= reserve`.** `commit` is the end of the last slot the consumer has settled, in order: published, or declared dead by the caller.

**RC-I15. One consumer.** A handle gives out its consumer once at a time (`ConsumerTaken`); that there is one across handles and processes is the caller's.

**RC-I16. A record held is delivered again, not lost.** The consumer releases the record it returned on its next `pop`; a consumer dropped while holding one leaves it in the ring, and the next consumer returns it.

**RC-I17. A claimed slot's length word is never zero.** The producer that claims a slot stores its length with bit 31 set, straight after the claim, before it writes a payload byte.

**RC-I18. A dead slot is skipped only when the caller says so.** `pop_or_skip_stale(is_stale)` calls `is_stale(p)` only for a slot that is claimed and not published, and treats that slot as padding only when it returns true. If the slot's length word is still zero, its extent is unknown and the consumer stops with `StaleExtentUnknown` (e.5).

**RC-I19. A consumer never reads outside the region.** A state no producer writes, a length word without bit 31, or a slot that would leave the data area stops the consumer with `CorruptSlot`; it returns `None` for ever after and `fault()` says why.

**RC-I20. What an empty ring accepts.** A slot of at most `capacity / 2` bytes is accepted by an empty ring wherever the write position is; between `capacity / 2` and `capacity` it depends on the position, because the padding to the end counts against the room (RC-I10).

**RC-I21. `unsafe` is confined.** Only `ring.rs` has it; each block carries a `// SAFETY:` comment naming the invariant.

## d. Interfaces

### d.1 The record (`record.rs`)

```rust
pub const FIXED: usize = 36;
pub const MAX_BODY: usize = 1 << 20;
pub const MAX_RECORD: usize = MAX_BODY + 64 * 1024;

pub struct Record<'a> { pub seq: u64, pub ts: i64, pub subject: u32, pub msg_id: &'a str,
                        pub headers: Vec<(&'a str, &'a str)>, pub body: &'a [u8] }
pub enum Decoded<'a> { Record(Record<'a>, usize), Incomplete, Corrupt(Corruption) }

pub fn check_fits(msg_id: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(), Error>;
pub fn encoded_len(msg_id: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<usize, Error>;
pub fn encode_into(buf: &mut [u8], seq: u64, ts: i64, subject: u32, msg_id: &str,
                   headers: &[(&str, &str)], body: &[u8]) -> Result<usize, Error>;  // bytes written
pub fn encode(seq: u64, ts: i64, subject: u32, msg_id: &str,
              headers: &[(&str, &str)], body: &[u8]) -> Result<Vec<u8>, Error>;
pub fn decode(buf: &[u8]) -> Decoded<'_>;
pub fn crc32c(data: &[u8]) -> u32;
```

### d.2 The ring (`ring.rs`)

```rust
impl<'a> Ring<'a> {
    pub fn init(region: &'a mut [u8]) -> Result<Ring<'a>, Error>;    // formats; zeroes the region
    pub fn attach(region: &'a mut [u8]) -> Result<Ring<'a>, Error>;  // checks the header, joins
    pub fn producer(&self) -> Producer<'_>;
    pub fn consumer(&self) -> Result<Consumer<'_>, Error>;           // RC-I15
    pub fn capacity(&self) -> u64;  pub fn reserved(&self) -> u64;  pub fn committed(&self) -> u64;
    pub fn released(&self) -> u64;  pub fn dropped(&self) -> u64;
}
impl Producer<'_> {                                                 // Send + Sync
    pub fn push(&self, record_bytes: &[u8]) -> Result<(), Full>;
    pub fn push_record(&self, seq: u64, ts: i64, subject: u32, msg_id: &str,
                       headers: &[(&str, &str)], body: &[u8]) -> Result<(), PushError>;
}
impl Consumer<'_> {                                                 // Send
    pub fn pop(&mut self) -> Option<&[u8]>;
    pub fn pop_or_skip_stale(&mut self, is_stale: impl Fn(u64) -> bool) -> Option<&[u8]>;
    pub fn dropped(&self) -> u64;  pub fn fault(&self) -> Option<Error>;  pub fn position(&self) -> u64;
}
```

`push` takes bytes already encoded; the ring does not look inside them, so any byte string up to the capacity is a slot. `push_record` encodes straight into the claimed slot with `encode_into`. `PushError` is `Full` or `Record(Error)` for parts that are not a record (nothing is claimed in that case).

### d.3 Errors

`Error` (`BodyTooLarge`, `MsgIdTooLong`, `TooManyHeaders`, `HeaderTooLong`, `RecordTooLarge`, `BufferTooSmall`, `Misaligned`, `RegionTooSmall`, `BadMagic`, `BadHeader`, `ConsumerTaken`, `CorruptSlot`, `StaleExtentUnknown`), `Corruption` (e.2), `Full`, `PushError`. All implement `Display`; with the `std` feature, `std::error::Error`.

## e. Data model, formats and state machines

### e.1 The record

| offset | size | field | |
|---|---|---|---|
| 0 | 4 | `len` | `u32`, the number of bytes after this field |
| 4 | 4 | `crc` | crc32c of bytes `[8, 4 + len)` |
| 8 | 8 | `seq` | `u64` |
| 16 | 8 | `ts` | `i64`, unix nanoseconds |
| 24 | 4 | `subject` | `u32`, opaque to this crate |
| 28 | 2 | `msg_id_len` | `u16` |
| 30 | 2 | `hdr_count` | `u16` |
| 32 | 4 | `body_len` | `u32`, at most `MAX_BODY` |
| 36 | `msg_id_len` | `msg_id` | UTF-8 |
| | | headers | `hdr_count` times: `u16` key length, key (UTF-8), `u16` value length, value (UTF-8) |
| | | `body` | `body_len` bytes |

A record with no id, no headers and no body is 36 bytes.

### e.2 What decode answers

In this order; the first that applies:

| Input | Outcome |
|---|---|
| fewer than 4 bytes | `Incomplete` |
| `len` outside `32 ..= MAX_RECORD` | `Corrupt(Length(len))` |
| fewer than `4 + len` bytes | `Incomplete` |
| stored crc is not the crc of `[8, 4 + len)` | `Corrupt(Crc { stored, actual })` |
| msg_id runs past the record | `Corrupt(MsgIdPastEnd)` |
| msg_id not UTF-8 | `Corrupt(MsgIdNotUtf8)` |
| a header's length word past the record (key or value side) | `Corrupt(HeaderPastEnd)` |
| a header key past the record | `Corrupt(HeaderKeyPastEnd)` |
| a header value past the record | `Corrupt(HeaderValuePastEnd)` |
| a header key or value not UTF-8 | `Corrupt(HeaderNotUtf8)` |
| the body does not end where the record does | `Corrupt(Body { body_len, at, total })` |
| otherwise | `Record(record, 4 + len)` |

Bytes after the record are not read. Each `Corruption` displays as the reference decoder's reason text.

### e.3 The ring's region

```
offset  size  field
0       8     magic     u64 = 0x4D52_4E47_5245_4331 ("MRNGREC1")
8       8     capacity  u64, the data area's size, a power of two, 64 ..= 2^30
64      8     reserve   AtomicU64, the next byte producers claim          (its own 64-byte line)
128     8     commit    AtomicU64, the end of the last settled slot      (its own line)
192     8     read      AtomicU64, what the consumer has released        (its own line)
256     8     dropped   AtomicU64, the pushes refused                    (its own line)
320     capacity       the data area
```

Words are native-endian: the region is shared by processes of one host. `init` picks the largest power of two, at most `2^30`, that fits after the header, zeroes the region, stores `capacity`, and stores the magic last with `Release`. `attach` refuses (`BadMagic`, `BadHeader`) a region whose magic is not the magic, whose capacity is not a power of two in range and within the region, or whose counters have `read > reserve` or `reserve - read > capacity`; `init` and `attach` refuse a region that does not start on a 64-byte boundary (`Misaligned`) or is smaller than `320 + 64` bytes (`RegionTooSmall`).

### e.4 The slot

```
offset  size  field
0       4     length   u32: payload length, with bit 31 set by the claimer (RC-I17)
4       4     state    u32: 0 being written (and what a released slot reads), 1 ready, 2 padding
8       n     payload  n bytes, then zeros to a multiple of 8
```

A slot is `8 + n` rounded up to a multiple of 8. A padding slot to the end of the data area has `n = tail - 8`, so that its size is the tail.

### e.5 The producer that dies

A producer that dies after its claim leaves a slot in state 0 for ever. The ring cannot know a producer is dead, and the caller can (a process that exited, a thread that was cancelled), so the ring asks: `pop_or_skip_stale(is_stale)` calls `is_stale(p)` for the position `p` of a claimed slot that is not ready, and a true answer makes the slot padding: the consumer takes its extent from the length word, zeroes it, and goes on.

The length word is stored right after the claim, as one store, with bit 31 set. A producer that dies between the compare-exchange and that store leaves a slot whose extent no one knows: the slot's size is not in the ring anywhere, and a guess would put the consumer in the middle of other slots. The consumer then stops with `Error::StaleExtentUnknown { pos }` in `fault()`, returns `None` for ever, and the owner of the ring has to format it again after its producers are gone. This is a window of one store, and the only one; it is stated here and not hidden.

## f. Algorithms and policies

**f.1 Encode.** `check_fits`, then the buffer's size, then one pass writing the fixed fields, the id, each header, the body, and last `len` and the crc. `encode` allocates a `Vec` of `encoded_len` bytes and calls `encode_into`.

**f.2 Decode.** e.2, with every offset checked against the record's end by saturating arithmetic, so a length field cannot wrap.

**f.3 Push.** A slot of `s = 8 + len` rounded up. If `s > capacity`: count, `Full`. Then loop: load `read` (`Acquire`), then `reserve` (`Acquire`), so that `read <= reserve`; `off = reserve & (capacity - 1)`, `tail = capacity - off`.

1. If `s <= tail`: if `reserve - read + s > capacity`, count and `Full`; else compare-exchange `reserve` to `reserve + s` (`AcqRel`). On success store the length word (`len | 1 << 31`, `Relaxed`) and return the claim; on failure loop.
2. Otherwise the slot would cross the end: if `reserve - read + tail > capacity`, count and `Full`; else compare-exchange `reserve` to `reserve + tail`. On success store the padding's length word (`tail - 8 | 1 << 31`) and its state 2 (`Release`), then loop: the slot is now claimed from offset 0 by the same rules.

After the claim, `push` copies the bytes and stores state 1 with `Release`; `push_record` encodes into the slot and does the same, or on the impossible failure of an encode its `encoded_len` accepted, publishes the slot as padding.

**f.4 Pop.** If the consumer has faulted, `None`. Release the slot held, if any: zero its `size` bytes, advance `pos`, store `read` with `Release`. Then loop on the slot at `pos`: load its state (`Acquire`) and length word.

- Ready or padding: validate the length word (bit 31 set, the slot inside the data area, else `CorruptSlot`), store `commit = pos + size` (`Release`). A ready slot is held and its payload returned; a padding slot is released and the loop continues.
- State 0: return `None`, unless the slot is claimed (`pos < reserve`, `Acquire`) and `is_stale(pos)`; then, if the length word is zero, fault `StaleExtentUnknown`, else treat it as padding.
- Any other state: fault `CorruptSlot`.

**f.5 Memory orders.** `Release` on publishing a state, on `read` and on `commit`; `Acquire` on reading a state and on a producer's loads of `read` and `reserve`; the length word and `dropped` are `Relaxed`; the compare-exchange on `reserve` is `AcqRel`.

## g. Concurrency within the component

Any number of producers push at once, from any threads and processes, without a lock: the only shared writes are the compare-exchange on `reserve`, the fetch-add on `dropped`, and each producer's own slot. One consumer reads; it alone writes `read`, `commit` and a released slot's bytes. A `Producer` is `Send` and `Sync`, a `Consumer` is `Send`, a `Ring` is `Send` and `Sync`; none holds a pointer into the region longer than the region's borrow. The region's borrow is `&'a mut [u8]`; two processes mapping one segment each hold their own.

A producer's pushes are ordered by its own program order and by claims, which are totally ordered by `reserve`; so one producer's records come out in the order pushed (RC-I13).

## h. Behaviour

An empty ring returns `None`. A ring that cannot take a record counts it and says `Full`; the producer decides whether to retry, drop or block, and the ring never does. After a `Full` caused by the record not fitting after its padding, the padding stays published and is skipped by the consumer. A held record is released by the next `pop`, including a `pop` that returns `None`, so a consumer that has read its last record frees its room by calling `pop` once more.

## i. Configuration

None. The ring's size is the region the caller maps; the limits are constants.

## j. Observability

`Ring::reserved`, `committed`, `released` and `dropped` are the four counters; `Consumer::fault` says why a consumer stopped.

## k. Tests

Integration tests under `crates/moruna-record/tests/`; each name starts with the test's id.

**RC-T1 `rc_t1_crc32c_matches_the_published_values`.** The check value `0xE3069283` and the published 32-byte vectors. RC-I1.

**RC-T2 `rc_t2_the_layout_is_byte_for_byte_as_documented`.** A record's bytes at every offset of e.1, by hand. RC-I1.

**RC-T3 `rc_t3_a_record_round_trips`.** Round trip; the empty record is 36 bytes; two records one after the other. RC-I1.

**RC-T4 `rc_t4_what_does_not_fit_is_refused_and_writes_nothing`.** Every limit of b.2 at, and one past, its edge; a short buffer is refused and untouched; an exact buffer is taken. And `rc_t4_errors_say_what_is_wrong_and_are_std_errors`. RC-I4.

**RC-T5 `rc_t5_a_subject_is_an_opaque_number`.** 0, 1, 2, 3, `2^31`, `u32::MAX` round trip. RC-I5.

**RC-T6 `rc_t6_encode_is_byte_identical_to_the_reference`.** 10,000 seeded random records (SplitMix64; ids 0 to 64 bytes with multi-byte characters, 0 to 8 headers, bodies 0 to 64 KiB, `seq`, `ts` and `subject` over their whole ranges) and one with a body of exactly `MAX_BODY`: `encode` equals the reference's, and `encode_into` into a larger buffer writes the same bytes and leaves the tail. RC-I2, RC-I4, RC-I6.

**RC-T7 `rc_t7_decode_agrees_with_the_reference_and_with_the_input`.** `decode` of each equals the reference's decode and the input's fields; trailing bytes do not matter. RC-I2.

**RC-T8 `rc_t8_every_single_byte_corruption_is_corrupt_in_both`.** Every byte of 200 records, corrupted: both decoders agree, neither returns a record, every corruption after the length field is `Corrupt`, one in the length field is `Corrupt` or `Incomplete`. RC-I3.

**RC-T9 `rc_t9_every_truncation_needs_more_bytes_in_both`.** Every truncation of those 200 and of the `MAX_BODY` record is `Incomplete` in both. RC-I3.

**RC-T10 `rc_t10_malformed_records_are_refused_for_the_same_reasons`.** Records built with a right crc and fields that do not add up, a non-UTF-8 id and header, a bad length, a bad crc, a long record in a short buffer: both decoders give the same outcome and reason text, and each reason of e.2 is reached. RC-I2, RC-I3.

**RC-T11 `rc_t11_check_fits_agrees_with_the_reference`.** Cases at the edge of every limit, including a record of exactly `MAX_RECORD`: the same verdict as the reference's `check_fits`, and what fits encodes to the reference's bytes. RC-I2.

**RC-T12 `rc_t12_records_come_out_in_the_order_they_went_in`.** Single-threaded order, `push_record` equals `encode`, an empty slice is a slot, an empty ring returns `None`. RC-I12.

**RC-T13 `rc_t13_a_slot_that_would_cross_the_end_is_padded_and_skipped`.** Paddings of 8 and 16 bytes (and others) after walking the write position to chosen places, the size of the claim, the record after the padding, several records after one; the ring goes round more than three times. RC-I9, RC-I8.

**RC-T14 `rc_t14_a_full_ring_refuses_counts_and_never_waits`.** Exactly the capacity is room, one more is `Full`, the count is exact, a record bigger than the ring is `Full`, a held record's room is not free until the next `pop`; a padding that cannot be claimed refuses the record and claims nothing; a padding that is claimed and a record that then does not fit leaves the padding to be skipped. RC-I10.

**RC-T15 `rc_t15_eight_producers_and_a_consumer_lose_and_repeat_nothing`.** 8 threads push 200,000 records each, with distinct `(thread, n)` ids, through a 1 MiB ring with a concurrent consumer. Every record arrives at most once, decodes with a valid crc and the body it was given, per-thread order is preserved, the set that arrived is exactly the set whose push returned `Ok`, and arrived plus `dropped` equals 1,600,000. RC-I11, RC-I12, RC-I13.

**RC-T16 `rc_t16_a_second_handle_on_the_same_region_sees_what_the_first_pushed`.** A second `Ring` attached to the same bytes pops what the first pushed, pushes into the same ring, and a third attached after both left resumes where the consumer released. RC-I7, RC-I16.

**RC-T17 `rc_t17_a_region_that_is_not_a_ring_is_refused`.** Garbage, zeros, one bit of the magic, a capacity that is not a power of two or is out of range or larger than the region, counters that are not a ring, a misaligned region, a region too small, for both `init` and `attach`; `init` rounds the data area down to a power of two and clears the region. RC-I7.

**RC-T18 `rc_t18_a_dead_producers_slot_is_skipped_when_the_caller_says_it_is_stale`.** A claimed, unpublished slot holds back the records after it; `pop` and a caller that says "live" wait; a caller that says "stale" for that position gets the next record, is asked once and only about that position, and the dead slot is settled; an empty ring is never asked about; a slot whose length was never written stops the consumer with `StaleExtentUnknown`, for good. RC-I12, RC-I14, RC-I18.

**RC-T19 `rc_t19_a_slot_no_producer_wrote_stops_the_consumer_loudly`.** A state no one writes, a length word without bit 31, a length that leaves the data area, a padding that leaves it, a stale slot that leaves it: each is `CorruptSlot { pos }` in `fault()`, and `pop` returns `None` after. RC-I19.

**RC-T20 `rc_t20_one_consumer_per_handle_and_a_dropped_one_gives_back_what_it_held`.** A second consumer is `ConsumerTaken`; after the first is dropped a new one gets the record the first had been holding. RC-I15, RC-I16.

**RC-T21 `rc_t21_push_record_encodes_in_place_and_refuses_what_is_not_a_record`.** An over-long body is `PushError::Record` and claims nothing, a record that fits the format and not the ring is `PushError::Full` and counted; the error types are `std` errors. RC-I6, RC-I10.

**RC-T22 `rc_t22_the_header_is_the_documented_layout`.** The magic, the capacity and every counter at the offsets of e.3, a slot's length word, state word and payload at the data area's start, `commit` and `read` moving as f.4 says, a released slot zero. RC-I7, RC-I8, RC-I14, RC-I17.

## l. Implementation notes for the agent

`ring.rs` holds all of the `unsafe`: `Core` is the region's address and the capacity, and its few methods (`counter`, `word`, `payload`, `zero`) are the only places a pointer becomes a reference or bytes are written. Everything else in the file is safe code over them. Keep it that way: a new access to the region is a new method on `Core` with its own `// SAFETY:` comment.

`tests/reference.rs` is the frozen reference; change it never, and a test that fails against it is a bug in the crate. The tests alias one block of memory as several `&mut [u8]` to stand for several processes mapping one segment; they touch the bytes directly only while no other thread does.

Size a ring for the records it carries: RC-I20 says a record above half the data area may be refused by an empty ring.

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KB4 (the record layout, byte for byte) | RC-I1, RC-I2 | RC-T1, RC-T2, RC-T3, RC-T6, RC-T7, RC-T10, RC-T11 |
| KB4 (decode's three outcomes, mirrored) | RC-I3 | RC-T8, RC-T9, RC-T10 |
| KB4 (the limits, `check_fits`) | RC-I4, RC-I5 | RC-T4, RC-T5, RC-T11 |
| KB4 (no allocation on the producer path) | RC-I6 | RC-T6, RC-T21 |
| KB4 (the region's header, offsets only) | RC-I7, RC-I8, RC-I17 | RC-T16, RC-T17, RC-T22 |
| KB4 (claim, padding, publish, never block) | RC-I9, RC-I10, RC-I11 | RC-T13, RC-T14, RC-T15 |
| KB4 (consumer order, exactly once) | RC-I12, RC-I13, RC-I14, RC-I15, RC-I16 | RC-T12, RC-T15, RC-T18, RC-T20 |
| KB4 (process restart, the stale skip) | RC-I18, RC-I19 | RC-T18, RC-T19 |
| CONTRIBUTING (`unsafe` with a `// SAFETY:` comment, in a module its design permits) | RC-I21 | the gate's clippy and review |

## o. Deferred (post-v1)

**RC-O1. A producer's death between its claim and its length store.** A one-store window leaves a slot of unknown extent (e.5); closing it needs a claim that carries the slot's size in the same atomic step as the position, which a single 64-bit `reserve` cannot do. Until then the consumer stops loudly and the owner formats the ring again.

**RC-O2. Producers learning the consumer's death.** The ring has no liveness protocol; a ring nobody reads fills and refuses. The caller supervises.
