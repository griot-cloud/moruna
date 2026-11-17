//! A program's coverage counters as console lines.
//!
//! A dump is three kinds of line, each one console write of at most [`LINE_MAX`] bytes, so
//! that the kernel prints it whole and lines from other processes fall only between them:
//!
//! ```text
//! #### usage 1 b INSTANCE SEQ PROGRAM PROCESS ADDR COUNT WAKES
//! #### usage 1 d INSTANCE SEQ INDEX V,V,,V
//! #### usage 1 e INSTANCE SEQ LINES NONZERO CHECKSUM
//! ```
//!
//! `INSTANCE` names one running process (16 hex digits), `SEQ` its dump, `ADDR` the address
//! of its first counter (hex) and `COUNT` how many 8-byte counters it has. A `d` line carries
//! the counters from `INDEX` on, in hex, a zero as an empty field; a run of eight zeros or
//! more ends the line. Counters left out are zero. The `e` line closes the dump with the
//! number of `d` lines, of nonzero counters and an FNV-1a hash over each nonzero counter's
//! index and value, so a dump cut short or mixed with another is refused.
//!
//! Each counter is read once, so the hash is over the values printed even while other
//! threads keep counting.

/// What starts every line of a dump.
pub const PREFIX: &str = "#### usage 1 ";

/// The longest line a dump writes, newline included: under the kernel's 4096-byte console
/// write.
pub const LINE_MAX: usize = 4000;

/// A run of this many zero counters ends a `d` line.
const GAP: usize = 8;

/// The most counters a decoded dump may claim, so a corrupt line cannot ask for unbounded
/// memory: 128 MiB of counters.
pub const COUNT_MAX: usize = 16 << 20;

/// The facts a dump starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    /// The program's binary name.
    pub program: &'a str,
    /// The process's name, as its creator gave it.
    pub process: &'a str,
    /// One running process.
    pub instance: u64,
    /// This process's dump number.
    pub seq: u64,
    /// The address of its first counter.
    pub addr: u64,
    /// How many 8-byte counters it has.
    pub count: usize,
    /// How many of its waits returned to it: the events it handled.
    pub wakes: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn mix(hash: u64, index: usize, value: u64) -> u64 {
    fnv(
        fnv(hash, &(index as u64).to_le_bytes()),
        &value.to_le_bytes(),
    )
}

/// A line being built in the caller's buffer.
struct Line<'b> {
    buf: &'b mut [u8],
    len: usize,
}

impl Line<'_> {
    fn room(&self) -> usize {
        self.buf.len() - self.len
    }

    fn push(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.room());
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    /// A name, with anything but `[A-Za-z0-9._-]` as `_`, at most 64 bytes, never empty.
    fn name(&mut self, name: &str) {
        if name.is_empty() {
            self.push(b"?");
        }
        for b in name.bytes().take(64) {
            let ok = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
            self.push(&[if ok { b } else { b'_' }]);
        }
    }

    fn hex(&mut self, mut v: u64, width: usize) {
        let mut digits = [0u8; 16];
        let mut n = 0;
        while v != 0 || n < width.max(1) {
            digits[15 - n] = b"0123456789abcdef"[(v & 15) as usize];
            v >>= 4;
            n += 1;
            if n == 16 {
                break;
            }
        }
        self.push(&digits[16 - n..]);
    }

    fn dec(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[19 - n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
            if v == 0 {
                break;
            }
        }
        self.push(&digits[20 - n..]);
    }

    fn start(&mut self, kind: u8, instance: u64, seq: u64) {
        self.len = 0;
        self.push(PREFIX.as_bytes());
        self.push(&[kind, b' ']);
        self.hex(instance, 16);
        self.push(b" ");
        self.dec(seq);
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// The next nonzero counter at or after `from`, each counter read once.
fn next_nonzero(
    from: usize,
    count: usize,
    counter: &impl Fn(usize) -> u64,
) -> Option<(usize, u64)> {
    (from..count).find_map(|i| match counter(i) {
        0 => None,
        v => Some((i, v)),
    })
}

/// The smallest buffer [`encode`] writes a dump in: room for a `b` line's two names.
pub const BUF_MIN: usize = 512;

/// Writes one dump of `header.count` counters, `counter(i)` giving counter `i`, as lines built
/// in `buf` (at most [`LINE_MAX`] bytes of it) and passed to `emit` one at a time. Each counter
/// is read once. Returns the number of `d` lines, or `None`, with nothing written, when `buf`
/// is shorter than [`BUF_MIN`]. A program passes a static buffer, so a thread's small stack
/// never holds a line.
pub fn encode(
    header: &Header<'_>,
    counter: impl Fn(usize) -> u64,
    buf: &mut [u8],
    emit: &mut impl FnMut(&[u8]),
) -> Option<u64> {
    if buf.len() < BUF_MIN {
        return None;
    }
    let limit = buf.len().min(LINE_MAX);
    let mut line = Line {
        buf: &mut buf[..limit],
        len: 0,
    };
    line.start(b'b', header.instance, header.seq);
    line.push(b" ");
    line.name(header.program);
    line.push(b" ");
    line.name(header.process);
    line.push(b" ");
    line.hex(header.addr, 1);
    line.push(b" ");
    line.dec(header.count as u64);
    line.push(b" ");
    line.dec(header.wakes);
    line.push(b"\n");
    emit(line.as_bytes());

    let mut lines = 0u64;
    let mut nonzero = 0u64;
    let mut hash = FNV_OFFSET;
    let mut next = next_nonzero(0, header.count, &counter);
    while let Some((start, value)) = next {
        line.start(b'd', header.instance, header.seq);
        line.push(b" ");
        line.dec(start as u64);
        line.push(b" ");
        line.hex(value, 1);
        hash = mix(hash, start, value);
        nonzero += 1;
        let mut last = start;
        loop {
            next = next_nonzero(last + 1, header.count, &counter);
            let Some((at, value)) = next else { break };
            // `at - last - 1` zeros lie between; a long run starts a new line, as does a
            // value that would not fit with its commas, its 16 digits and the newline.
            if at - last > GAP || line.room() < (at - last) + 16 + 1 {
                break;
            }
            for _ in last..at {
                line.push(b",");
            }
            line.hex(value, 1);
            hash = mix(hash, at, value);
            nonzero += 1;
            last = at;
        }
        line.push(b"\n");
        emit(line.as_bytes());
        lines += 1;
    }

    line.start(b'e', header.instance, header.seq);
    line.push(b" ");
    line.dec(lines);
    line.push(b" ");
    line.dec(nonzero);
    line.push(b" ");
    line.hex(hash, 16);
    line.push(b"\n");
    emit(line.as_bytes());
    Some(lines)
}

#[cfg(feature = "alloc")]
pub use decode::*;

#[cfg(feature = "alloc")]
mod decode {
    use alloc::{
        collections::BTreeMap,
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    use super::{COUNT_MAX, FNV_OFFSET, PREFIX, mix};

    /// One whole dump, checked against its `e` line.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Dump {
        pub program: String,
        pub process: String,
        pub instance: u64,
        pub seq: u64,
        pub addr: u64,
        pub wakes: u64,
        pub counters: Vec<u64>,
    }

    /// What a serial log held: the whole dumps, and how many were begun but cut short or
    /// failed their check.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Decoded {
        pub dumps: Vec<Dump>,
        pub broken: usize,
    }

    struct Partial {
        dump: Dump,
        lines: u64,
        nonzero: u64,
        hash: u64,
        bad: bool,
    }

    fn hex(field: &str) -> Option<u64> {
        if field.is_empty() || field.len() > 16 {
            return None;
        }
        u64::from_str_radix(field, 16).ok()
    }

    /// The dumps in a serial log. A dump line may follow other text on its line (a process
    /// that wrote without a newline); everything before the prefix is ignored.
    pub fn decode(text: &str) -> Decoded {
        let mut open: BTreeMap<(u64, u64), Partial> = BTreeMap::new();
        let mut out = Decoded::default();
        for raw in text.lines() {
            let Some(at) = raw.find(PREFIX) else { continue };
            let line = raw[at + PREFIX.len()..].trim_end_matches('\r');
            let fields: Vec<&str> = line.split(' ').collect();
            let (Some(kind), Some(instance), Some(seq)) = (
                fields.first(),
                fields.get(1).and_then(|f| hex(f)),
                fields.get(2).and_then(|f| f.parse::<u64>().ok()),
            ) else {
                out.broken += 1;
                continue;
            };
            let key = (instance, seq);
            match (*kind, fields.len()) {
                ("b", 8) => {
                    let count = fields[6].parse::<usize>().ok().filter(|c| *c <= COUNT_MAX);
                    let (Some(addr), Some(count), Some(wakes)) =
                        (hex(fields[5]), count, fields[7].parse::<u64>().ok())
                    else {
                        out.broken += 1;
                        continue;
                    };
                    let partial = Partial {
                        dump: Dump {
                            program: fields[3].to_string(),
                            process: fields[4].to_string(),
                            instance,
                            seq,
                            addr,
                            wakes,
                            counters: vec![0; count],
                        },
                        lines: 0,
                        nonzero: 0,
                        hash: FNV_OFFSET,
                        bad: false,
                    };
                    if open.insert(key, partial).is_some() {
                        out.broken += 1;
                    }
                }
                ("d", 5) => {
                    let Some(p) = open.get_mut(&key) else {
                        out.broken += 1;
                        continue;
                    };
                    p.lines += 1;
                    let Some(start) = fields[3].parse::<usize>().ok() else {
                        p.bad = true;
                        continue;
                    };
                    for (offset, value) in fields[4].split(',').enumerate() {
                        if value.is_empty() {
                            continue;
                        }
                        let index = start.saturating_add(offset);
                        match (hex(value), p.dump.counters.get_mut(index)) {
                            (Some(v), Some(slot)) if v != 0 => {
                                *slot = v;
                                p.hash = mix(p.hash, index, v);
                                p.nonzero += 1;
                            }
                            _ => p.bad = true,
                        }
                    }
                }
                ("e", 6) => {
                    let Some(p) = open.remove(&key) else {
                        out.broken += 1;
                        continue;
                    };
                    let whole = !p.bad
                        && fields[3].parse::<u64>().ok() == Some(p.lines)
                        && fields[4].parse::<u64>().ok() == Some(p.nonzero)
                        && hex(fields[5]) == Some(p.hash);
                    if whole {
                        out.dumps.push(p.dump);
                    } else {
                        out.broken += 1;
                    }
                }
                _ => out.broken += 1,
            }
        }
        out.broken += open.len();
        out
    }

    /// Each running process's last whole dump: its counters are cumulative, so the last
    /// one holds every earlier one.
    pub fn latest(dumps: &[Dump]) -> Vec<&Dump> {
        let mut last: BTreeMap<u64, &Dump> = BTreeMap::new();
        for dump in dumps {
            let keep = last.get(&dump.instance).is_none_or(|d| d.seq < dump.seq);
            if keep {
                last.insert(dump.instance, dump);
            }
        }
        last.into_values().collect()
    }

    /// One program's counters summed over its processes.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Program {
        pub program: String,
        /// The processes that ran it, by name, and how many of each.
        pub processes: BTreeMap<String, u64>,
        /// Waits that returned to any of them.
        pub wakes: u64,
        pub addr: u64,
        pub counters: Vec<u64>,
    }

    /// The last dump of each process, summed per program. Two dumps of one program name
    /// with different counter layouts come from different builds, which is refused.
    pub fn programs(dumps: &[Dump]) -> Result<BTreeMap<String, Program>, String> {
        let mut out: BTreeMap<String, Program> = BTreeMap::new();
        for dump in latest(dumps) {
            let program = out.entry(dump.program.clone()).or_insert_with(|| Program {
                program: dump.program.clone(),
                processes: BTreeMap::new(),
                wakes: 0,
                addr: dump.addr,
                counters: vec![0; dump.counters.len()],
            });
            if program.addr != dump.addr || program.counters.len() != dump.counters.len() {
                return Err(alloc::format!(
                    "{}: two counter layouts ({:#x}, {} and {:#x}, {}): two builds of one program",
                    dump.program,
                    program.addr,
                    program.counters.len(),
                    dump.addr,
                    dump.counters.len()
                ));
            }
            *program.processes.entry(dump.process.clone()).or_insert(0) += 1;
            program.wakes = program.wakes.saturating_add(dump.wakes);
            for (sum, v) in program.counters.iter_mut().zip(&dump.counters) {
                *sum = sum.saturating_add(*v);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{string::String, vec, vec::Vec};

    use super::*;

    fn header(count: usize) -> Header<'static> {
        Header {
            program: "virtio-blk",
            process: "driver 00:05.0",
            instance: 0x1234,
            seq: 3,
            addr: 0x40_1000,
            count,
            wakes: 9,
        }
    }

    fn lines_of(header: &Header<'_>, counters: &[u64]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut buf = [0u8; LINE_MAX + 100];
        encode(header, |i| counters[i], &mut buf, &mut |b: &[u8]| {
            assert!(b.len() <= LINE_MAX);
            assert_eq!(b.last(), Some(&b'\n'));
            lines.push(String::from_utf8(b.to_vec()).unwrap());
        });
        lines
    }

    #[test]
    fn a_dump_is_its_three_kinds_of_line() {
        let counters = [0, 5, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
        let lines = lines_of(&header(counters.len()), &counters);
        assert_eq!(
            lines[0],
            "#### usage 1 b 0000000000001234 3 virtio-blk driver_00_05.0 401000 15 9\n"
        );
        // A run of eight zeros or more starts a new line.
        assert_eq!(lines[1], "#### usage 1 d 0000000000001234 3 1 5,,,10\n");
        assert_eq!(lines[2], "#### usage 1 d 0000000000001234 3 14 7\n");
        assert!(lines[3].starts_with("#### usage 1 e 0000000000001234 3 2 3 "));
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn an_empty_name_is_a_question_mark_and_zero_counters_send_no_data() {
        let mut h = header(4);
        h.process = "";
        let lines = lines_of(&h, &[0, 0, 0, 0]);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(" virtio-blk ? 401000 4 9"));
        assert!(lines[1].contains(" e 0000000000001234 3 0 0 "));
    }

    #[test]
    fn each_counter_is_read_once() {
        let reads = std::cell::RefCell::new(vec![0u32; 40]);
        let counter = |i: usize| {
            reads.borrow_mut()[i] += 1;
            (i % 3) as u64 * 1000
        };
        let mut buf = [0u8; BUF_MIN];
        assert_eq!(
            encode(&header(40), counter, &mut buf, &mut |_: &[u8]| {}),
            Some(1)
        );
        assert!(reads.borrow().iter().all(|n| *n == 1));
    }

    #[test]
    fn a_buffer_too_small_writes_nothing() {
        let mut buf = [0u8; BUF_MIN - 1];
        let mut emitted = 0;
        let lines = encode(&header(4), |_| 1, &mut buf, &mut |_: &[u8]| emitted += 1);
        assert_eq!((lines, emitted), (None, 0));
    }

    #[test]
    fn a_small_buffer_makes_short_lines() {
        let counters: Vec<u64> = (0..500).map(|i| i + 1).collect();
        let mut buf = [0u8; BUF_MIN];
        let mut lines = Vec::new();
        encode(
            &header(500),
            |i| counters[i],
            &mut buf,
            &mut |b: &[u8]| {
                assert!(b.len() <= BUF_MIN);
                lines.push(String::from_utf8(b.to_vec()).unwrap());
            },
        );
        assert!(lines.len() > 4);
        #[cfg(feature = "alloc")]
        assert_eq!(decode(&lines.concat()).dumps[0].counters, counters);
    }

    #[test]
    fn a_long_run_of_large_counters_spans_lines() {
        let counters: Vec<u64> = (0..2000).map(|i| u64::MAX - i).collect();
        let lines = lines_of(&header(counters.len()), &counters);
        assert!(lines.len() > 10);
        assert!(lines.iter().all(|l| l.len() <= LINE_MAX));
    }

    #[cfg(feature = "alloc")]
    mod decoding {
        use super::*;

        #[test]
        fn a_dump_round_trips_amid_other_output() {
            let counters: Vec<u64> = (0..5000u64)
                .map(|i| if i % 7 == 0 { i * 31 + 1 } else { 0 })
                .collect();
            let lines = lines_of(&header(counters.len()), &counters);
            let mut log = String::from("kk: booting\n");
            for (n, line) in lines.iter().enumerate() {
                if n == 2 {
                    // Another process's partial line before a dump line.
                    log.push_str("linux: half a line ");
                }
                log.push_str(line);
                if n == 1 {
                    log.push_str("node: service 3 started\r\n");
                }
            }
            let decoded = decode(&log);
            assert_eq!(decoded.broken, 0);
            assert_eq!(decoded.dumps.len(), 1);
            let d = &decoded.dumps[0];
            assert_eq!(d.counters, counters);
            assert_eq!(
                (d.program.as_str(), d.process.as_str(), d.addr, d.wakes),
                ("virtio-blk", "driver_00_05.0", 0x40_1000, 9)
            );
        }

        #[test]
        fn a_cut_or_altered_dump_is_refused() {
            let counters = [1u64, 2, 3, 0, 4];
            let lines = lines_of(&header(5), &counters);
            // Cut: no `e` line.
            let cut: String = lines[..lines.len() - 1].concat();
            assert_eq!(
                decode(&cut),
                Decoded {
                    dumps: vec![],
                    broken: 1
                }
            );
            // Altered: a value changed.
            let altered = lines.concat().replace(" 0 1,2,3,,4", " 0 1,2,9,,4");
            assert_eq!(decode(&altered).broken, 1);
            // A data line for no begun dump, a short line and an unknown kind.
            let stray =
                "#### usage 1 d 00000000000000ff 1 0 5\n#### usage 1 b\n#### usage 1 z 01 1\n";
            assert_eq!(decode(stray).broken, 3);
            // A value past the claimed count, and a malformed index.
            let past = lines.concat().replace(" 0 1,2,3,,4", " 0 1,2,3,,4,5");
            assert_eq!(decode(&past).broken, 1);
            let index = lines.concat().replace(" 0 1,2,3,,4", " x 1,2,3,,4");
            assert_eq!(decode(&index).broken, 1);
        }

        #[test]
        fn a_begin_line_must_be_whole_and_bounded() {
            for bad in [
                "#### usage 1 b 01 1 p q zz 4 0\n",
                "#### usage 1 b 01 1 p q 10 99999999999 0\n",
                "#### usage 1 b 01 1 p q 10 4 x\n",
            ] {
                assert_eq!(decode(bad).broken, 1, "{bad}");
            }
            // The same dump begun twice: the first is lost.
            let twice = "#### usage 1 b 01 1 p q 10 4 0\n#### usage 1 b 01 1 p q 10 4 0\n";
            assert_eq!(decode(twice).broken, 2);
            // An end for no dump, and a hex field too long.
            assert_eq!(decode("#### usage 1 e 01 1 0 0 00\n").broken, 1);
            assert_eq!(
                decode("#### usage 1 e 01234567890123456 1 0 0 00\n").broken,
                1
            );
        }

        #[test]
        fn the_last_dump_of_each_process_is_summed_per_program() {
            let mut a1 = header(3);
            a1.seq = 1;
            let mut a2 = header(3);
            a2.seq = 2;
            let mut b = header(3);
            b.instance = 0x99;
            b.process = "driver 00:06.0";
            b.wakes = 1;
            let mut other = header(2);
            other.program = "linux";
            other.instance = 0x55;
            other.addr = 0x50_0000;
            let mut log = String::new();
            for (h, c) in [
                (&a2, [5u64, 0, 1].as_slice()),
                (&a1, [1, 0, 0].as_slice()),
                (&b, [2, 2, 2].as_slice()),
                (&other, [1, 1].as_slice()),
            ] {
                log.extend(lines_of(h, c));
            }
            let decoded = decode(&log);
            assert_eq!(decoded.dumps.len(), 4);
            assert_eq!(latest(&decoded.dumps).len(), 3);
            let per = programs(&decoded.dumps).unwrap();
            let blk = &per["virtio-blk"];
            assert_eq!(blk.counters, vec![7, 2, 3]);
            assert_eq!(blk.wakes, 10);
            assert_eq!(blk.processes.len(), 2);
            assert_eq!(per["linux"].counters, vec![1, 1]);

            // The same program name with another layout is two builds.
            let mut moved = header(3);
            moved.instance = 0x77;
            moved.addr = 0x40_2000;
            log.extend(lines_of(&moved, &[1, 1, 1]));
            let err = programs(&decode(&log).dumps).unwrap_err();
            assert!(err.contains("two builds"), "{err}");
        }
    }
}
