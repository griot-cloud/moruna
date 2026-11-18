//! How many processes a function's stage runs (system design 3E, item 6.3e), one rule for every
//! engine that starts a function as processes of its own: the Moruna kernel's image task
//! (`moruna_micro::task`) and the Linux image kind (`moruna_sandbox`).
//!
//! - **One instance per CPU of the job:** the job's `budget.cpu`, rounded up, or the node's CPUs
//!   when the job names none ([`cpus`]).
//! - **Capped** by [`MAX`] and by the ceiling the function declares, `MORUNA_PROCESSES=N` in its
//!   image: its environment ([`declared`]) or, for a native function, its program's ELF note
//!   ([`noted`]). A function that keeps state across batches, a running total say, declares how
//!   many instances it can run. A ceiling, not a count: parallelism follows the job, never a
//!   value fixed when the image was packaged ([`count`]).
//! - **Bounded by memory:** each instance is funded with an equal share of what the run has left
//!   past its arena, at least a function's least. The run starts as many as that funds, at least
//!   one a stage, and is refused only when one does not fit ([`fund`]). Found by 6.2 run 3: Score's
//!   function ran as one process on a 4-CPU node, 82 s of 93 s.

/// The name an image's environment declares its ceiling under.
pub const ENV: &str = "MORUNA_PROCESSES";

/// The most processes one stage runs.
pub const MAX: usize = 64;

/// The job's CPUs: its `budget.cpu` rounded up when it names a positive, finite one, otherwise
/// `node`, the CPUs of the machine it runs on; at least one ([`count`] caps it).
pub fn cpus(budget: Option<f64>, node: usize) -> usize {
    match budget {
        Some(cpu) if cpu.is_finite() && cpu > 0.0 => {
            // A float past `usize` saturates; [`count`] caps it at [`MAX`].
            (cpu.ceil() as usize).max(1)
        }
        _ => node.max(1),
    }
}

/// The ceiling `env` (each `NAME=VALUE`) declares under [`ENV`], or `None` when it declares
/// none. A value that is not a whole number from 1 to [`MAX`] is refused, in words.
pub fn declared(env: &[String]) -> Result<Option<usize>, String> {
    let prefix = format!("{ENV}=");
    env.iter()
        .find_map(|e| e.strip_prefix(prefix.as_str()))
        .map(|v| match v.parse::<usize>() {
            Ok(n) if (1..=MAX).contains(&n) => Ok(n),
            _ => Err(format!(
                "{ENV}={v}: a function declares a ceiling of 1 to {MAX} processes"
            )),
        })
        .transpose()
}

/// The owner of the ELF note a native function declares its ceiling in: a note of type
/// [`NOTE_TYPE`] in the section `.note.moruna`, which the user target's linker scripts keep as the
/// program's `PT_NOTE` (system design 5A, 5B: `moruna_function::processes!(N)`,
/// `MORUNA_PROCESSES(N);`).
pub const NOTE_OWNER: &str = "Moruna";

/// The type of that note; its description is `MORUNA_PROCESSES=N`, as an image's environment
/// entry is written.
pub const NOTE_TYPE: u32 = 1;

/// The program header of a note segment.
const PT_NOTE: u32 = 4;

/// The ceiling a native program's ELF note declares ([`NOTE_OWNER`], [`NOTE_TYPE`]), or `None`
/// when it has none: the first such note of its `PT_NOTE` segments, read as [`declared`] reads an
/// environment entry. Its file's SHA-256, which registration signs, covers it. A note segment past
/// the file's bytes, a note cut short, or a note of Moruna's that declares anything else, is
/// refused in words.
pub fn noted(elf: &[u8]) -> Result<Option<usize>, String> {
    let short = || String::from("its program headers lie past its bytes");
    let u16_at = |at: usize| {
        elf.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| {
        elf.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let u64_at = |at: usize| {
        elf.get(at..at + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
            .and_then(|v| usize::try_from(v).ok())
    };
    if elf.get(..4) != Some(b"\x7fELF".as_slice())
        || elf.get(4) != Some(&2)
        || elf.get(5) != Some(&1)
    {
        return Err("it is not a 64-bit little-endian ELF program".into());
    }
    let phoff = u64_at(32).ok_or_else(short)?;
    let size = usize::from(u16_at(54).ok_or_else(short)?);
    let count = usize::from(u16_at(56).ok_or_else(short)?);
    for i in 0..count {
        let at = phoff
            .checked_add(i.checked_mul(size).ok_or_else(short)?)
            .ok_or_else(short)?;
        if u32_at(at).ok_or_else(short)? != PT_NOTE {
            continue;
        }
        let (offset, len, align) = (
            u64_at(at + 8).ok_or_else(short)?,
            u64_at(at + 32).ok_or_else(short)?,
            u64_at(at + 48).ok_or_else(short)?,
        );
        let notes = offset
            .checked_add(len)
            .and_then(|end| elf.get(offset..end))
            .ok_or("its note segment lies past its bytes")?;
        if let Some(ceiling) = moruna_note(notes, if align == 8 { 8 } else { 4 })? {
            return Ok(Some(ceiling));
        }
    }
    Ok(None)
}

/// The ceiling Moruna's note among `notes` (one note segment, its entries aligned to `align`)
/// declares, if it holds one.
fn moruna_note(notes: &[u8], align: usize) -> Result<Option<usize>, String> {
    let cut = || String::from("its Moruna note is cut short");
    let word = |at: usize| {
        notes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let up = |n: usize| n.div_ceil(align) * align;
    let mut at = 0;
    while at + 12 <= notes.len() {
        let (name_len, desc_len, kind) = (
            word(at).ok_or_else(cut)?,
            word(at + 4).ok_or_else(cut)?,
            word(at + 8).ok_or_else(cut)?,
        );
        let name_at = at + 12;
        let desc_at = up(name_at.checked_add(name_len).ok_or_else(cut)?);
        let desc_end = desc_at.checked_add(desc_len).ok_or_else(cut)?;
        let (Some(name), Some(desc)) = (
            notes.get(name_at..name_at + name_len),
            notes.get(desc_at..desc_end),
        ) else {
            return Err(cut());
        };
        if name.strip_suffix(b"\0").unwrap_or(name) == NOTE_OWNER.as_bytes()
            && kind == NOTE_TYPE as usize
        {
            let text = std::str::from_utf8(desc)
                .map_err(|_| String::from("its Moruna note is not text"))?
                .trim_end_matches('\0');
            return match declared(&[text.to_string()])? {
                Some(ceiling) => Ok(Some(ceiling)),
                None => Err(format!(
                    "its Moruna note declares `{text}`, where it declares {ENV}=N"
                )),
            };
        }
        at = up(desc_end);
    }
    Ok(None)
}

/// The ceiling a function declares in its image: the least of its environment's ([`declared`])
/// and, for a native program, its ELF note's ([`noted`]).
pub fn ceiling(env: &[String], elf: Option<&[u8]>) -> Result<Option<usize>, String> {
    let noted = elf.map(noted).transpose()?.flatten();
    Ok(match (declared(env)?, noted) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    })
}

/// The instances a stage runs, before memory bounds them: one per CPU of `cpus` (at least one),
/// at most [`MAX`], and at most the `ceiling` the function declares.
pub fn count(cpus: usize, ceiling: Option<usize>) -> usize {
    cpus.clamp(1, MAX).min(ceiling.unwrap_or(MAX)).max(1)
}

/// What the run's memory funds: each instance's share, and how many instances of each stage it
/// starts, in the order the stages were given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funded {
    /// Each instance's share, in whole pages, at least the least a stage's instance needs.
    pub share: u64,
    /// The instances each stage starts: at least one, at most what it asked for.
    pub counts: Vec<usize>,
}

impl Funded {
    /// The instances funded in all.
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }
}

/// Funds the stages that asked for `wanted` instances each out of `room`, the bytes the run has
/// left past its arena and what else it funded: equal shares in whole `page`s, each at least
/// `minimum`. Every instance asked for when the room funds them all; otherwise as many as it
/// funds, at least one a stage, given out a stage at a time in turn, so no stage runs more than
/// one past another short of what it asked. `None` when no stage asked for any; refused when
/// the room does not fund one instance a stage, or the geometry is not one (a page that is not
/// a power of two, a zero minimum).
pub fn fund(
    room: u64,
    wanted: &[usize],
    page: u64,
    minimum: u64,
) -> Result<Option<Funded>, String> {
    if !page.is_power_of_two() || minimum == 0 {
        return Err("invalid native task reservation geometry".into());
    }
    let wanted: Vec<usize> = wanted.iter().map(|&n| n.max(1)).collect();
    if wanted.is_empty() {
        return Ok(None);
    }
    let share = |n: usize| (room / n as u64) & !(page - 1);
    let asked: usize = wanted.iter().sum();
    if share(asked) >= minimum {
        return Ok(Some(Funded {
            share: share(asked),
            counts: wanted,
        }));
    }
    // The most instances the room funds at `minimum` each.
    let mut most = usize::try_from(room / minimum)
        .unwrap_or(usize::MAX)
        .min(asked);
    while most > 0 && share(most) < minimum {
        most -= 1;
    }
    if most < wanted.len() {
        return Err(format!(
            "{} stages of processes need at least {minimum} bytes for each of their instances, \
             one a stage at the least; the run has {room} bytes after funded allocations",
            wanted.len()
        ));
    }
    let mut counts = vec![1usize; wanted.len()];
    let mut left = most - wanted.len();
    while left > 0 {
        let mut gave = false;
        for (n, want) in counts.iter_mut().zip(&wanted) {
            if left > 0 && *n < *want {
                *n += 1;
                left -= 1;
                gave = true;
            }
        }
        if !gave {
            break;
        }
    }
    let total = counts.iter().sum();
    Ok(Some(Funded {
        share: share(total),
        counts,
    }))
}

/// The run report's note of a stage memory left short of what the rule asked for: `N of M
/// instances: the share funds N`, after the stage's name.
pub fn short_note(stage: &str, funded: usize, asked: usize) -> Option<String> {
    (funded < asked)
        .then(|| format!("{stage}: {funded} of {asked} instances: the share funds {funded}"))
}

/// What the build says of a stage's count ([`count`]): `instances` for the job's `cpus`, and
/// why it is fewer when it is.
pub fn count_note(instances: usize, cpus: usize) -> String {
    let counted = match instances {
        1 => String::from("1 instance"),
        n => format!("{n} instances"),
    };
    if instances >= cpus {
        format!("{counted}, one per CPU of the job")
    } else if instances == MAX {
        format!("{counted}, the most a stage runs, for the job's {cpus} CPUs")
    } else {
        format!("{counted} for the job's {cpus} CPUs: the function declares at most {instances}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const PAGE: u64 = 4096;

    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    /// 6.3e's defect, as the rule: a function that declares nothing, in a job of 4 CPUs, runs 4
    /// instances; it ran one, whatever the job had (`layout::processes`, 6.2 run 3).
    #[test]
    fn a_function_that_declares_nothing_runs_one_instance_per_cpu_of_the_job() {
        let ceiling = declared(&env(&["PATH=/", "MORUNA_PLANE=rings"])).unwrap();
        assert_eq!(ceiling, None);
        assert_eq!(count(cpus(Some(4.0), 16), ceiling), 4);
        // No budget.cpu: the node's.
        assert_eq!(count(cpus(None, 8), ceiling), 8);
    }

    #[test]
    fn the_job_s_cpus_are_its_budget_rounded_up_or_the_node_s() {
        assert_eq!(cpus(Some(2.5), 16), 3);
        assert_eq!(cpus(Some(0.25), 16), 1);
        assert_eq!(cpus(Some(4.0), 1), 4);
        assert_eq!(cpus(None, 0), 1);
        // A budget that is not a positive number is the node's (the job's check refuses it).
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(cpus(Some(bad), 6), 6, "{bad}");
        }
        assert_eq!(count(cpus(Some(1e30), 1), None), MAX);
    }

    #[test]
    fn a_declared_ceiling_caps_the_count_and_never_raises_it() {
        assert_eq!(declared(&env(&["MORUNA_PROCESSES=1"])), Ok(Some(1)));
        assert_eq!(count(4, Some(1)), 1);
        assert_eq!(count(4, Some(8)), 4, "a ceiling, not a count");
        assert_eq!(count(16, Some(8)), 8);
        assert_eq!(count(200, None), MAX);
        assert_eq!(count(0, None), 1);
        assert_eq!(declared(&env(&["MORUNA_PROCESSES=64"])), Ok(Some(64)));
        for bad in ["0", "65", "x", "", "-1"] {
            let refused = declared(&env(&[&format!("MORUNA_PROCESSES={bad}")])).unwrap_err();
            assert!(refused.contains("1 to 64"), "{refused}");
        }
        // The first declaration is the one, as an environment's first entry wins.
        assert_eq!(
            declared(&env(&["MORUNA_PROCESSES=2", "MORUNA_PROCESSES=3"])),
            Ok(Some(2))
        );
    }

    /// One ELF note: its owner, type and description, each padded to `align`.
    fn note(owner: &str, kind: u32, desc: &str, align: usize) -> Vec<u8> {
        let pad = |b: &mut Vec<u8>| b.resize(b.len().div_ceil(align) * align, 0);
        let mut b = Vec::new();
        b.extend_from_slice(&(owner.len() as u32 + 1).to_le_bytes());
        b.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        b.extend_from_slice(&kind.to_le_bytes());
        b.extend_from_slice(owner.as_bytes());
        b.push(0);
        pad(&mut b);
        b.extend_from_slice(desc.as_bytes());
        pad(&mut b);
        b
    }

    /// An ELF header, a loaded segment's program header and a `PT_NOTE` one over `notes`, which
    /// follow the headers, of `align`.
    fn elf(notes: &[u8], align: u64) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&62u16.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        let header = |ty: u32, flags: u32, offset: u64, len: u64, align: u64| {
            let mut h = [0u8; 56];
            h[..4].copy_from_slice(&ty.to_le_bytes());
            h[4..8].copy_from_slice(&flags.to_le_bytes());
            h[8..16].copy_from_slice(&offset.to_le_bytes());
            h[16..24].copy_from_slice(&0x40_0000u64.to_le_bytes());
            h[32..40].copy_from_slice(&len.to_le_bytes());
            h[40..48].copy_from_slice(&len.to_le_bytes());
            h[48..56].copy_from_slice(&align.to_le_bytes());
            h
        };
        b.extend_from_slice(&header(1, 5, 0, 4096, 4096));
        b.extend_from_slice(&header(PT_NOTE, 4, 64 + 2 * 56, notes.len() as u64, align));
        b.extend_from_slice(notes);
        b
    }

    /// A native function's ceiling is its program's Moruna note (PM, 2026-11-18): the note
    /// `moruna_function::processes!(1)` and `MORUNA_PROCESSES(1);` write.
    #[test]
    fn a_native_function_declares_its_ceiling_in_its_elf_note() {
        let ours = note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=1", 4);
        assert_eq!(noted(&elf(&ours, 4)), Ok(Some(1)));
        // Behind another owner's note, and with 8-byte alignment.
        let gnu = note("GNU", 1, "\u{1}\u{2}\u{3}\u{4}", 8);
        let both = [gnu, note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=12", 8)].concat();
        assert_eq!(noted(&elf(&both, 8)), Ok(Some(12)));
        // Moruna's note of another type is not this one.
        assert_eq!(noted(&elf(&note("Moruna", 2, "x", 4), 4)), Ok(None));
        // An empty note segment, as the linker scripts leave one when the program declares
        // nothing, and no note segment at all.
        assert_eq!(noted(&elf(&[], 4)), Ok(None));
        let mut none = elf(&[], 4);
        none[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(noted(&none), Ok(None));
        // A description with its NUL, as C's string writes it.
        let nul = note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=3\0", 4);
        assert_eq!(noted(&elf(&nul, 4)), Ok(Some(3)));
    }

    #[test]
    fn a_note_that_is_not_one_is_refused_in_words() {
        let says = |b: Vec<u8>, what: &str| {
            let why = noted(&b).unwrap_err();
            assert!(why.contains(what), "{why}");
        };
        says(
            elf(&note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=0", 4), 4),
            "1 to 64",
        );
        says(
            elf(&note("Moruna", NOTE_TYPE, "THREADS=2", 4), 4),
            "THREADS=2",
        );
        says(
            elf(&note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=", 4), 4),
            "1 to 64",
        );
        let mut bad = note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=1", 4);
        bad[24] = 0xff;
        says(elf(&bad, 4), "not text");
        let ours = note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=1", 4);
        let mut cut = elf(&ours, 4);
        cut.truncate(cut.len() - 8);
        says(cut, "past its bytes");
        // A note whose lengths run past its segment.
        let mut long = ours.clone();
        long[4..8].copy_from_slice(&400u32.to_le_bytes());
        says(elf(&long, 4), "cut short");
        says(b"#!/bin/sh\n".to_vec(), "not a 64-bit");
        says(elf(&ours, 4)[..40].to_vec(), "program headers");
        // More program headers than its bytes hold, read past an empty note segment.
        let mut headers = elf(&[], 4);
        headers[56..58].copy_from_slice(&9u16.to_le_bytes());
        says(headers, "program headers");
    }

    #[test]
    fn the_ceiling_is_the_least_the_image_declares() {
        let env = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let one = elf(&note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=1", 4), 4);
        let none = elf(&[], 4);
        assert_eq!(
            ceiling(&env(&["MORUNA_PROCESSES=4"]), Some(&one)),
            Ok(Some(1))
        );
        assert_eq!(
            ceiling(&env(&["MORUNA_PROCESSES=4"]), Some(&none)),
            Ok(Some(4))
        );
        assert_eq!(ceiling(&env(&[]), Some(&one)), Ok(Some(1)));
        assert_eq!(ceiling(&env(&["MORUNA_PROCESSES=2"]), None), Ok(Some(2)));
        assert_eq!(ceiling(&env(&[]), None), Ok(None));
        let two = elf(&note("Moruna", NOTE_TYPE, "MORUNA_PROCESSES=2", 4), 4);
        assert_eq!(
            ceiling(&env(&["MORUNA_PROCESSES=8"]), Some(&two)),
            Ok(Some(2))
        );
        assert!(ceiling(&env(&["MORUNA_PROCESSES=x"]), Some(&one)).is_err());
        assert!(ceiling(&env(&[]), Some(b"no")).is_err());
    }

    #[test]
    fn memory_that_funds_every_instance_funds_every_instance() {
        let funded = fund(1 << 30, &[4], PAGE, 29 * MIB).unwrap().unwrap();
        assert_eq!(funded.counts, vec![4]);
        assert_eq!(funded.share, 256 * MIB);
        assert_eq!(funded.total(), 4);
        assert_eq!(fund(1 << 30, &[], PAGE, 29 * MIB).unwrap(), None);
        // As the share the run gave before (`child_share`): page-aligned, equal.
        let funded = fund(352_361_673 - 117_440_512, &[2], PAGE, 16 * MIB)
            .unwrap()
            .unwrap();
        assert_eq!(funded.share, 117_456_896);
    }

    /// PM, 2026-11-18 (Q1): memory bounds the instances and never fails a job that ran before.
    #[test]
    fn memory_bounds_the_instances_and_refuses_only_when_one_does_not_fit() {
        // A 2 GiB node of 16 CPUs: 200 MiB past the arena funds 6 of 16 instances of 29 MiB.
        let funded = fund(200 * MIB, &[16], PAGE, 29 * MIB).unwrap().unwrap();
        assert_eq!(funded.counts, vec![6]);
        assert!(funded.share >= 29 * MIB && 6 * funded.share <= 200 * MIB);
        assert_eq!(
            short_note("gate-score", 6, 16).as_deref(),
            Some("gate-score: 6 of 16 instances: the share funds 6")
        );
        assert_eq!(short_note("gate-score", 4, 4), None);
        // Exactly one fits.
        let funded = fund(29 * MIB, &[16], PAGE, 29 * MIB).unwrap().unwrap();
        assert_eq!(funded.counts, vec![1]);
        // Not one: refused, in words.
        let refused = fund(29 * MIB - 1, &[16], PAGE, 29 * MIB).unwrap_err();
        assert!(refused.contains("one a stage"), "{refused}");
        assert!(refused.contains(&(29 * MIB - 1).to_string()), "{refused}");
    }

    #[test]
    fn the_build_says_why_a_stage_runs_fewer_than_the_job_s_cpus() {
        assert_eq!(count_note(4, 4), "4 instances, one per CPU of the job");
        assert_eq!(
            count_note(1, 4),
            "1 instance for the job's 4 CPUs: the function declares at most 1"
        );
        assert_eq!(
            count_note(MAX, 100),
            "64 instances, the most a stage runs, for the job's 100 CPUs"
        );
    }

    /// The shares the run gave stages of one process each before 6.3e (`child_share`) are the
    /// same: equal, page-aligned, never more than the room, refused under the least.
    #[test]
    fn stages_of_one_process_get_the_shares_they_had() {
        let share = |room: u64, stages: usize| {
            fund(room, &vec![1; stages], PAGE, 16 * MIB).map(|f| f.map(|f| f.share))
        };
        // The controller's pool, two stages.
        assert_eq!(share(114_780_240, 2), Ok(Some(57_389_056)));
        // 128 MiB less a 32 MiB arena and 16 MiB of funded engines, three stages.
        assert_eq!(share((128 - 32 - 16) * MIB, 3), Ok(Some(27_959_296)));
        assert_eq!(share(32 * MIB, 2), Ok(Some(16 * MIB)));
        assert_eq!(share(0, 0), Ok(None));
        // Exhausted or sub-minimum pools never become a task ceiling.
        for room in [0, 32 * MIB - 1, 32 * MIB - 4096] {
            assert!(share(room, 2).is_err(), "{room}");
        }
    }

    #[test]
    fn several_stages_share_the_room_in_turn_one_each_at_the_least() {
        // 7 instances' room for 4 + 8 + 1 asked: 1 each, then one a stage in turn.
        let funded = fund(7 * 32 * MIB, &[4, 8, 1], PAGE, 32 * MIB)
            .unwrap()
            .unwrap();
        assert_eq!(funded.counts, vec![3, 3, 1]);
        assert_eq!(funded.share, 32 * MIB);
        // Two stages, one instance's room: refused, since each needs one.
        assert!(fund(40 * MIB, &[4, 4], PAGE, 32 * MIB).is_err());
        // A stage that asked for none asks for one.
        assert_eq!(
            fund(1 << 30, &[0], PAGE, MIB).unwrap().unwrap().counts,
            vec![1]
        );
    }

    #[test]
    fn a_geometry_that_is_not_one_is_refused() {
        for (page, minimum) in [(3000, 16 * MIB), (PAGE, 0)] {
            let refused = fund(1 << 30, &[2], page, minimum).unwrap_err();
            assert!(refused.contains("reservation geometry"), "{refused}");
        }
    }

    /// The page rounding can leave the room one instance short of `room / minimum`.
    #[test]
    fn the_most_funded_is_counted_after_the_page_rounding() {
        // 3 x 10 pages + 3 bytes: room / minimum is 3, and each share rounds down to 10 pages.
        let minimum = 10 * PAGE;
        let funded = fund(3 * minimum + 3, &[5], PAGE, minimum).unwrap().unwrap();
        assert_eq!(funded.counts, vec![3]);
        // A minimum that is not whole pages: 3 shares round below it, so 2 are funded.
        let funded = fund(3 * (minimum + 1), &[5], PAGE, minimum + 1)
            .unwrap()
            .unwrap();
        assert_eq!(funded.counts, vec![2]);
        assert!(funded.share > minimum);
    }
}
