//! The network server's meter (work item 6.3ab): where its bytes and its time go, counted
//! always, and said on the console in the node's `KE2 netstack: meter` line.
//!
//! The node read its stores at about 55 MB/s however many requests were in flight, and nothing
//! on it said where the time went. The meter counts, for the server's whole life:
//!
//! - **its loop:** the turns, the time of the loop's own work between waits (`busy`: the
//!   polls, the cards' sends), the requests its clients sent and the time their handlers took
//!   (`requests`), and the time taking its cards' answers (`card`);
//! - **smoltcp:** the polls, and the time each spends taking frames into TCP (`ingress`),
//!   answering what waits (`service`, which holds TLS and the clients' copies below) and
//!   making frames of what TCP sends (`egress`);
//! - **TLS:** ciphertext taken from TCP and given to it, plaintext opened (rustls's record
//!   processing, its decryption) and sealed, and the time each took;
//! - **its clients:** the bytes copied into their loans and out of them, the time the copies
//!   took with their mappings, and the receives answered against the receives tried;
//! - **its cards** ([`CardCounts`], one a card): the answers to its receives, the frames and
//!   bytes in and out, and the frames its interface dropped.
//!
//! A count costs an addition. A time costs two reads of the server's clock (on a node, the
//! clock page, with no call) around a batch of work: a turn, a poll's three parts, a TLS
//! socket's records, a client's request. Never one a frame.
//!
//! [`Meter::line`] says the counts, all from the server's start, at most every
//! [`PERIOD_NS`] while bytes move, and once every TLS socket has closed after some were open
//! (a job's end). A line between two others is the difference of the two.

use alloc::{format, string::String};

/// The least time between two lines while bytes move: a line on the serial console holds its
/// CPU for a few milliseconds.
pub const PERIOD_NS: u64 = 10_000_000_000;

/// The server's counts, from its start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The server loop's turns, and the time of its own work between waits: the polls, the
    /// cards' sends.
    pub turns: u64,
    pub busy_ns: u64,
    /// The clients' requests, and the time their handlers took.
    pub requests: u64,
    pub requests_ns: u64,
    /// The time taking the cards' answers: their frames read, decoded and queued.
    pub card_ns: u64,
    /// Polls of the stack, and the time in their three parts.
    pub polls: u64,
    pub ingress_ns: u64,
    pub service_ns: u64,
    pub egress_ns: u64,
    /// Ciphertext taken from TCP, and given to it.
    pub tls_in_bytes: u64,
    pub tls_out_bytes: u64,
    /// Plaintext opened by rustls (its record processing, decryption with it), and the time.
    pub opened_bytes: u64,
    pub open_ns: u64,
    /// Plaintext sealed by rustls, and the time.
    pub sealed_bytes: u64,
    pub seal_ns: u64,
    /// Bytes copied into the clients' loans, the receives answered with them, the receives
    /// tried (a pending one is tried at each poll), and the time of the tries.
    pub to_clients_bytes: u64,
    pub receives: u64,
    pub receive_tries: u64,
    pub to_clients_ns: u64,
    /// Bytes copied out of the clients' loans, the sends answered with them, and the time of
    /// the tries.
    pub from_clients_bytes: u64,
    pub sends: u64,
    pub from_clients_ns: u64,
    /// TLS sockets at the last poll.
    pub tls_sockets: u64,
}

/// A card's counts, as the server's link to it sees them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CardCounts {
    /// Answers to the server's receives, and the frames and bytes they carried.
    pub answers: u64,
    pub frames_in: u64,
    pub bytes_in: u64,
    /// The server's sends, and the frames and bytes they carried.
    pub sends: u64,
    pub frames_out: u64,
    pub bytes_out: u64,
}

impl CardCounts {
    /// Each count of `self` and `other` added.
    pub fn plus(&self, other: &CardCounts) -> CardCounts {
        CardCounts {
            answers: self.answers + other.answers,
            frames_in: self.frames_in + other.frames_in,
            bytes_in: self.bytes_in + other.bytes_in,
            sends: self.sends + other.sends,
            frames_out: self.frames_out + other.frames_out,
            bytes_out: self.bytes_out + other.bytes_out,
        }
    }
}

/// What the server's line says beside its own counts: its cards'.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Beside {
    /// Every card's counts, added.
    pub cards: CardCounts,
    /// Frames the interfaces dropped, their queues full.
    pub dropped: u64,
}

/// What the process has been charged, read from the core only for a line that is due: the
/// reading is a call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charged {
    /// The CPU time charged to the server's account, when the core says it.
    pub cpu_ns: Option<u64>,
    /// The large blocks its heap has mapped, each a memory object made, mapped and unmapped
    /// (`moruna_sys::heap_large_blocks`), when it has that heap.
    pub heap_maps: Option<u64>,
}

/// The counts and the clock they are timed with.
#[derive(Clone, Debug)]
pub struct Meter {
    clock: fn() -> u64,
    pub counts: Counts,
    /// When the last line was said, and the bytes moved by then.
    said_at: Option<u64>,
    said_moved: u64,
    /// Whether a TLS socket was open since the last line.
    tls_seen: bool,
}

impl Meter {
    /// A meter timing with `clock`, in nanoseconds.
    pub fn new(clock: fn() -> u64) -> Meter {
        Meter {
            clock,
            counts: Counts::default(),
            said_at: None,
            said_moved: 0,
            tls_seen: false,
        }
    }

    /// Times from now on with `clock`: on a node, the clock page's.
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = clock;
    }

    /// The clock now.
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    /// The time since `start`, on the clock.
    pub fn since(&self, start: u64) -> u64 {
        self.now().saturating_sub(start)
    }

    /// The TLS sockets the last poll found.
    pub fn tls_sockets(&mut self, count: u64) {
        self.counts.tls_sockets = count;
        self.tls_seen |= count > 0;
    }

    /// The loop's own work in a turn, from `woke` (its return from the dispatcher), ends now.
    pub fn turned(&mut self, woke: u64) {
        self.counts.turns += 1;
        self.counts.busy_ns += self.since(woke);
    }

    /// The line at `now`, if one is due: bytes moved and [`PERIOD_NS`] has passed since the
    /// last, or every TLS socket has closed since some were open. `charged` is read only then.
    pub fn line(
        &mut self,
        now: u64,
        beside: &Beside,
        charged: impl FnOnce() -> Charged,
    ) -> Option<String> {
        let c = &self.counts;
        let moved = beside.cards.bytes_in
            + beside.cards.bytes_out
            + c.to_clients_bytes
            + c.from_clients_bytes;
        let drained = self.tls_seen && c.tls_sockets == 0;
        let period = self
            .said_at
            .is_none_or(|at| now.saturating_sub(at) >= PERIOD_NS);
        if moved == self.said_moved || !(period || drained) {
            return None;
        }
        self.said_at = Some(now);
        self.said_moved = moved;
        self.tls_seen = c.tls_sockets > 0;
        Some(say(now, c, beside, &charged()))
    }
}

/// Microseconds of `ns`.
fn us(ns: u64) -> u64 {
    ns / 1_000
}

/// The line: `key=value` pairs, times in microseconds, bytes in bytes.
fn say(now: u64, c: &Counts, b: &Beside, charged: &Charged) -> String {
    let known = |v: Option<u64>| v.map_or_else(|| String::from("unknown"), |v| format!("{v}"));
    format!(
        "KE2 netstack: meter at_ms={} cpu_us={} heap_maps={} busy_us={} turns={} \
         requests={} requests_us={} card_us={} \
         card_answers={} card_in={}f/{}B card_sends={} card_out={}f/{}B dropped={} \
         polls={} ingress_us={} service_us={} egress_us={} \
         tls_in={}B tls_out={}B opened={}B open_us={} sealed={}B seal_us={} \
         to_clients={}B receives={}/{} to_clients_us={} \
         from_clients={}B sends={} from_clients_us={} tls_sockets={}",
        now / 1_000_000,
        known(charged.cpu_ns.map(us)),
        known(charged.heap_maps),
        us(c.busy_ns),
        c.turns,
        c.requests,
        us(c.requests_ns),
        us(c.card_ns),
        b.cards.answers,
        b.cards.frames_in,
        b.cards.bytes_in,
        b.cards.sends,
        b.cards.frames_out,
        b.cards.bytes_out,
        b.dropped,
        c.polls,
        us(c.ingress_ns),
        us(c.service_ns),
        us(c.egress_ns),
        c.tls_in_bytes,
        c.tls_out_bytes,
        c.opened_bytes,
        us(c.open_ns),
        c.sealed_bytes,
        us(c.seal_ns),
        c.to_clients_bytes,
        c.receives,
        c.receive_tries,
        us(c.to_clients_ns),
        c.from_clients_bytes,
        c.sends,
        us(c.from_clients_ns),
        c.tls_sockets,
    )
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::cell::Cell;

    std::thread_local! {
        static NOW: Cell<u64> = const { Cell::new(0) };
    }

    /// A clock that moves 1,000 ns at each read.
    fn ticking() -> u64 {
        NOW.with(|n| {
            n.set(n.get() + 1_000);
            n.get()
        })
    }

    fn moved(bytes: u64) -> Beside {
        Beside {
            cards: CardCounts {
                bytes_in: bytes,
                ..CardCounts::default()
            },
            ..Beside::default()
        }
    }

    #[test]
    fn a_turn_is_timed_on_the_meters_clock() {
        let mut m = Meter::new(ticking);
        let woke = m.now();
        m.turned(woke);
        assert_eq!(m.counts.turns, 1);
        assert_eq!(m.counts.busy_ns, 1_000);
        assert_eq!(m.since(m.now()), 1_000);
    }

    #[test]
    fn the_clock_can_be_set() {
        fn stopped() -> u64 {
            7
        }
        let mut m = Meter::new(ticking);
        m.set_clock(stopped);
        assert_eq!(m.now(), 7);
        assert_eq!(m.since(7), 0);
    }

    #[test]
    fn a_line_is_said_once_bytes_move_then_at_most_every_period() {
        let mut m = Meter::new(ticking);
        // Nothing moved: no line, however long.
        assert_eq!(m.line(0, &Beside::default(), Charged::default), None);
        let first = m
            .line(1_000_000, &moved(10), Charged::default)
            .expect("the first line");
        assert!(first.starts_with("KE2 netstack: meter at_ms=1 cpu_us=unknown heap_maps=unknown "));
        assert!(first.contains(" card_in=0f/10B "), "{first}");
        // More bytes, within the period: nothing.
        assert_eq!(
            m.line(2_000_000, &moved(20), || panic!("read only for a line")),
            None
        );
        // The period passed but nothing more moved: nothing.
        let later = 1_000_000 + PERIOD_NS;
        assert_eq!(m.line(later, &moved(10), Charged::default), None);
        // The period passed and bytes moved: a line.
        assert!(m.line(later, &moved(30), Charged::default).is_some());
    }

    #[test]
    fn a_line_is_said_when_the_last_tls_socket_closes() {
        let mut m = Meter::new(ticking);
        m.tls_sockets(2);
        assert!(m.line(1_000_000, &moved(10), Charged::default).is_some());
        // Within the period, sockets still open: nothing.
        assert_eq!(m.line(2_000_000, &moved(20), Charged::default), None);
        // Every TLS socket closed: the line at once, the period aside.
        m.tls_sockets(0);
        let end = m
            .line(3_000_000, &moved(30), Charged::default)
            .expect("the job's end");
        assert!(end.ends_with(" tls_sockets=0"), "{end}");
        // Said once: closed sockets alone say nothing more.
        assert_eq!(m.line(4_000_000, &moved(40), Charged::default), None);
    }

    #[test]
    fn the_line_says_every_count() {
        let mut m = Meter::new(ticking);
        m.counts = Counts {
            turns: 1,
            busy_ns: 2_000,
            requests: 31,
            requests_ns: 32_000,
            card_ns: 33_000,
            polls: 3,
            ingress_ns: 4_000,
            service_ns: 5_000,
            egress_ns: 6_000,
            tls_in_bytes: 7,
            tls_out_bytes: 8,
            opened_bytes: 9,
            open_ns: 10_000,
            sealed_bytes: 11,
            seal_ns: 12_000,
            to_clients_bytes: 13,
            receives: 14,
            receive_tries: 15,
            to_clients_ns: 16_000,
            from_clients_bytes: 17,
            sends: 18,
            from_clients_ns: 19_000,
            tls_sockets: 20,
        };
        let cards = CardCounts {
            answers: 21,
            frames_in: 22,
            bytes_in: 23,
            sends: 24,
            frames_out: 25,
            bytes_out: 26,
        };
        let beside = Beside {
            cards: cards.plus(&CardCounts::default()),
            dropped: 27,
        };
        let charged = || Charged {
            cpu_ns: Some(28_000),
            heap_maps: Some(30),
        };
        assert_eq!(
            m.line(29_000_000, &beside, charged).unwrap(),
            "KE2 netstack: meter at_ms=29 cpu_us=28 heap_maps=30 busy_us=2 turns=1 requests=31 \
             requests_us=32 card_us=33 card_answers=21 \
             card_in=22f/23B card_sends=24 card_out=25f/26B dropped=27 polls=3 ingress_us=4 \
             service_us=5 egress_us=6 tls_in=7B tls_out=8B opened=9B open_us=10 sealed=11B \
             seal_us=12 to_clients=13B receives=14/15 to_clients_us=16 from_clients=17B \
             sends=18 from_clients_us=19 tls_sockets=20"
        );
    }

    #[test]
    fn card_counts_add() {
        let a = CardCounts {
            answers: 1,
            frames_in: 2,
            bytes_in: 3,
            sends: 4,
            frames_out: 5,
            bytes_out: 6,
        };
        assert_eq!(
            a.plus(&a),
            CardCounts {
                answers: 2,
                frames_in: 4,
                bytes_in: 6,
                sends: 8,
                frames_out: 10,
                bytes_out: 12,
            }
        );
    }
}
