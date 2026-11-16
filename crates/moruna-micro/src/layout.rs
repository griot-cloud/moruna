//! The rules an image stage on the kernel follows that are arithmetic, kept apart so every
//! target tests them (SDD 30 f.5, f.6, f.11).

/// A page of the data plane.
pub const PAGE: u64 = 4096;
/// The smallest window a stage's task is granted for one output record.
pub const WINDOW_MIN: u64 = 1 << 20;
/// The largest: the kernel's largest memory object (`moruna_abi::limits::MEMORY_MAX_BYTES`).
pub const WINDOW_MAX: u64 = 256 << 20;
/// How many times its input record a task's output record may be, within the bounds above:
/// a task that widens a batch more than this is ended, saying how much room it had.
pub const WINDOW_GROWTH: u64 = 2;

/// The least account a stage's task is started in: a process, its threads, its image's
/// writable pages and its heap (SDD 30 f.11).
pub const TASK_MIN: u64 = 16 << 20;

/// The limit of the account a stage's task is started in (SDD 30 f.11), carved out of the
/// engine's own: the stage's ceiling, as a Linux node gives its child's cgroup, but never
/// more than half of what is left in the engine's account (`room`), so the engine keeps
/// the other half for its own arena and for the tasks of the stages after this one; in
/// whole pages. `None` when that is less than [`TASK_MIN`].
pub fn task_limit(ceiling: u64, room: u64) -> Option<u64> {
    let limit = ceiling.min(room / 2) & !(PAGE - 1);
    (limit >= TASK_MIN).then_some(limit)
}

/// The name of a stage's task's account: `task.` and the stage's name, in the kernel's
/// alphabet (`[a-z0-9._-]`, other bytes as `-`), at most 32 bytes.
pub fn account_name(stage: &str) -> String {
    let mut name = String::from("task.");
    for c in stage.chars() {
        if name.len() == 32 {
            break;
        }
        name.push(match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '.' | '_' | '-') => c,
            _ => '-',
        });
    }
    name
}

/// `bytes` rounded up to whole pages; `None` past `u64`.
pub fn pages(bytes: u64) -> Option<u64> {
    bytes.checked_add(PAGE - 1).map(|b| b & !(PAGE - 1))
}

/// The window granted for the record a task writes in answer to a record of `input` bytes:
/// [`WINDOW_GROWTH`] times it and a megabyte more, in whole pages, within [`WINDOW_MIN`] and
/// [`WINDOW_MAX`].
pub fn window_for(input: u64) -> u64 {
    let want = input
        .saturating_mul(WINDOW_GROWTH)
        .saturating_add(WINDOW_MIN);
    pages(want)
        .unwrap_or(WINDOW_MAX)
        .clamp(WINDOW_MIN, WINDOW_MAX)
}

/// A record's length: its framing, then each body buffer at its offset, which
/// `moruna_kernel::ipc::encode_framing` places on pages. IPC declares the body through
/// the final buffer rounded to eight bytes, including its zero padding.
pub fn record_len(framing: u64, placed: &[(u64, u64)]) -> Option<u64> {
    let end = placed.iter().try_fold(framing, |end, (offset, len)| {
        offset.checked_add(*len).map(|e| e.max(end))
    })?;
    end.checked_add(7).map(|end| end & !7)
}

/// The name of the port a task calls as `number` (SDD 30 f.7): the ports its entry lists, in
/// name order, numbered from 1.
pub fn port_name(names: &[String], number: u64) -> Option<&str> {
    let i = usize::try_from(number.checked_sub(1)?).ok()?;
    names.get(i).map(String::as_str)
}

/// The text a log record carries, as the lines of the stream it came from: each record is one
/// line, without its newline (SDD 29 f.7).
pub fn line(body: &[u8]) -> Vec<u8> {
    let mut l = body.to_vec();
    l.push(b'\n');
    l
}

/// What one of a record's bytes ranges is written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// These bytes at this offset of the record.
    Bytes(u64, &'a [u8]),
    /// So many zeros at this offset.
    Zeros(u64, u64),
}

/// How a record of `len` bytes is written: its framing at 0, each body buffer at its offset
/// (in order, none overlapping, all within `len`), and every gap zero, so the task sees no
/// byte the engine did not mean it to. `None` when the buffers do not fit.
pub fn pieces<'a>(len: u64, framing: &'a [u8], body: &[(u64, &'a [u8])]) -> Option<Vec<Piece<'a>>> {
    let mut out = vec![Piece::Bytes(0, framing)];
    let mut end = framing.len() as u64;
    if end > len {
        return None;
    }
    for (offset, bytes) in body {
        let stop = offset.checked_add(bytes.len() as u64)?;
        if *offset < end || stop > len {
            return None;
        }
        if *offset > end {
            out.push(Piece::Zeros(end, offset - end));
        }
        out.push(Piece::Bytes(*offset, bytes));
        end = stop;
    }
    if len > end {
        out.push(Piece::Zeros(end, len - end));
    }
    Some(out)
}

/// The sizes of a ring task's plane, from the stage's plane budget (SDD 30 f.6): its input
/// region a quarter of it, and two windows each an eighth, so the region, the rings object in
/// use and one handed back with its records still held fit the budget together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingSizes {
    /// The input region's bytes.
    pub region: u64,
    /// The windows of a rings object.
    pub windows: u64,
    /// Each window's bytes.
    pub window: u64,
}

/// The least region and window: a record of a small batch and its framing.
pub const RING_MIN: u64 = 1 << 20;

/// [`RingSizes`] for a plane of `budget` bytes, in whole pages.
pub fn ring_sizes(budget: u64) -> RingSizes {
    let quarter = (budget / 4) & !(PAGE - 1);
    let eighth = (budget / 8) & !(PAGE - 1);
    RingSizes {
        region: quarter.clamp(RING_MIN, WINDOW_MAX),
        windows: 2,
        window: eighth.clamp(RING_MIN, WINDOW_MAX),
    }
}

/// Whether the record a task put is kept where it is and handed on as it lies (no copy),
/// or copied into the run's arena so its window goes back at once: kept while fewer than
/// `windows - 1` records of the task's are still held downstream, so the task always has a
/// window to write its next record in, whatever downstream holds.
pub fn keep_in_place(held: u64, windows: u64) -> bool {
    held + 1 < windows
}

/// Whether an image declares the ring protocol in its environment (`MORUNA_PLANE=rings`):
/// the Python shim's layer sets it. Any other image is served on its descriptors 0 and 1.
pub fn declares_rings(env: &[String]) -> bool {
    env.iter().any(|e| e == "MORUNA_PLANE=rings")
}

/// The most processes one image stage runs.
pub const PROCESSES_MAX: usize = 64;

/// How many processes an image's stage runs, one interpreter each: the `MORUNA_PROCESSES` its
/// environment declares, or one.
pub fn processes(env: &[String]) -> Result<usize, String> {
    match env.iter().find_map(|e| e.strip_prefix("MORUNA_PROCESSES=")) {
        None => Ok(1),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if (1..=PROCESSES_MAX).contains(&n) => Ok(n),
            _ => Err(format!(
                "MORUNA_PROCESSES={v}: a stage runs 1 to {PROCESSES_MAX} processes"
            )),
        },
    }
}

/// Where the user target's linker script (`kernel/sys/user.ld`) puts a native program's first
/// segment, its text.
pub const NATIVE_BASE: u64 = 0x40_0000;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Whether `elf` (the whole program, or at least its ELF header and program headers) is a program of
/// the Moruna kernel's user target (`x86_64-moruna-user`), as an image entry declaring
/// `abi: moruna` says its command is: a static x86-64 executable laid out by the user linker
/// script, whose program headers are its loaded segments and nothing else, read and execute
/// first at 0x400000, then read only, then read and write. A Linux program has a stack header
/// (and a static musl one its notes and its thread-local storage), so none passes. `Err` says
/// what does not match.
pub fn native_elf(elf: &[u8]) -> Result<(), String> {
    if elf.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err("it is not an ELF program".into());
    }
    if elf.get(4) != Some(&2) || elf.get(5) != Some(&1) {
        return Err("it is not a 64-bit little-endian ELF program".into());
    }
    let short = || String::from("its ELF header ends early");
    let (kind, machine) = (
        u16_at(elf, 16).ok_or_else(short)?,
        u16_at(elf, 18).ok_or_else(short)?,
    );
    if kind != 2 {
        return Err(format!(
            "it is not a static executable (ELF type {kind}, where the user target links type 2)"
        ));
    }
    if machine != 62 {
        return Err(format!(
            "it is not an x86-64 program (ELF machine {machine})"
        ));
    }
    let phoff = u64_at(elf, 32).ok_or_else(short)?;
    let (size, count) = (
        u16_at(elf, 54).ok_or_else(short)?,
        u16_at(elf, 56).ok_or_else(short)?,
    );
    if size != 56 || !(1..=3).contains(&count) {
        return Err(format!(
            "it has {count} program headers of {size} bytes, where the user target's linker \
             script writes one to three of 56 (its loaded segments)"
        ));
    }
    let mut flags = Vec::new();
    for i in 0..u64::from(count) {
        let at = usize::try_from(phoff + i * 56)
            .map_err(|_| String::from("its program headers lie past its bytes"))?;
        let ty = u32_at(elf, at)
            .ok_or_else(|| String::from("its program headers lie past its bytes"))?;
        if ty != PT_LOAD {
            return Err(format!(
                "its program header {i} is of type {ty:#x}, not a loaded segment: the user \
                 target's linker script writes none other (a Linux program has a stack header)"
            ));
        }
        let f = u32_at(elf, at + 4).unwrap_or(0);
        let vaddr = u64_at(elf, at + 16).unwrap_or(0);
        if i == 0 && vaddr != NATIVE_BASE {
            return Err(format!(
                "its first segment is at {vaddr:#x}, where the user target's linker script puts \
                 its text at {NATIVE_BASE:#x}"
            ));
        }
        flags.push(f & (PF_R | PF_W | PF_X));
    }
    let order = [PF_R | PF_X, PF_R, PF_R | PF_W];
    let mut next = order.iter();
    if flags.first() != Some(&(PF_R | PF_X)) || !flags.iter().all(|f| next.any(|o| o == f)) {
        return Err(format!(
            "its segments' permissions are {flags:?}, where the user target's linker script \
             writes read and execute, then read, then read and write"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ELF header and its program headers: (type, flags, vaddr) each.
    fn elf(kind: u16, machine: u16, headers: &[(u32, u32, u64)]) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[16..18].copy_from_slice(&kind.to_le_bytes());
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        for (ty, flags, vaddr) in headers {
            let mut h = [0u8; 56];
            h[..4].copy_from_slice(&ty.to_le_bytes());
            h[4..8].copy_from_slice(&flags.to_le_bytes());
            h[16..24].copy_from_slice(&vaddr.to_le_bytes());
            b.extend_from_slice(&h);
        }
        b
    }

    #[test]
    fn p_a_native_program_is_the_user_target_s_layout_and_nothing_else() {
        let user = [
            (1, 5, NATIVE_BASE),
            (1, 4, NATIVE_BASE + 0x10000),
            (1, 6, NATIVE_BASE + 0x20000),
        ];
        assert_eq!(native_elf(&elf(2, 62, &user)), Ok(()));
        // An empty segment the linker leaves out.
        assert_eq!(native_elf(&elf(2, 62, &[user[0], user[2]])), Ok(()));
        assert_eq!(native_elf(&elf(2, 62, &user[..1])), Ok(()));
        let refused = |b: Vec<u8>, says: &str| {
            let why = native_elf(&b).unwrap_err();
            assert!(why.contains(says), "{why}");
        };
        refused(b"#!/bin/sh\n".to_vec(), "not an ELF");
        let mut class32 = elf(2, 62, &user);
        class32[4] = 1;
        refused(class32, "64-bit");
        refused(elf(2, 62, &user)[..17].to_vec(), "ends early");
        refused(elf(3, 62, &user), "ELF type 3");
        refused(elf(2, 183, &user), "machine 183");
        refused(elf(2, 62, &[]), "0 program headers");
        // A static musl program: its stack header.
        refused(
            elf(2, 62, &[user[0], (0x6474_e551, 6, 0)]),
            "type 0x6474e551",
        );
        // A Linux program's read-only segment first, at the same base.
        refused(elf(2, 62, &[(1, 4, NATIVE_BASE), user[0]]), "permissions");
        refused(elf(2, 62, &[(1, 5, 0x20_0000)]), "at 0x200000");
        refused(elf(2, 62, &[user[0], user[2], user[1]]), "permissions");
        refused(
            elf(2, 62, &[user[0], (1, 7, NATIVE_BASE + 0x10000)]),
            "permissions",
        );
        let mut cut = elf(2, 62, &user);
        cut.truncate(64 + 56);
        refused(cut, "past its bytes");
    }

    #[test]
    fn p_a_record_is_its_framing_its_buffers_and_zeros() {
        let framing = [1u8; 10];
        let a = [2u8; 8];
        let b = [3u8; 4];
        let p = pieces(4096 + 64, &framing, &[(4096, &a), (4096 + 16, &b)]).unwrap();
        assert_eq!(
            p,
            vec![
                Piece::Bytes(0, &framing),
                Piece::Zeros(10, 4086),
                Piece::Bytes(4096, &a),
                Piece::Zeros(4104, 8),
                Piece::Bytes(4112, &b),
                Piece::Zeros(4116, 44),
            ]
        );
        assert_eq!(
            pieces(10, &framing, &[]).unwrap(),
            vec![Piece::Bytes(0, &framing)]
        );
        assert_eq!(pieces(4, &framing, &[]), None, "framing past the record");
        assert_eq!(
            pieces(4100, &framing, &[(4096, &a)]),
            None,
            "a buffer past it"
        );
        assert_eq!(
            pieces(8192, &framing, &[(4, &a)]),
            None,
            "a buffer over the framing"
        );
        assert_eq!(pieces(8192, &framing, &[(u64::MAX, &a)]), None, "past u64");
    }

    #[test]
    fn p_ring_sizes_fit_the_budget() {
        const MIB: u64 = 1 << 20;
        let s = ring_sizes(1 << 30);
        assert_eq!(
            s,
            RingSizes {
                region: 256 * MIB,
                windows: 2,
                window: 128 * MIB
            }
        );
        assert!(s.region + 2 * s.windows * s.window <= 1 << 30);
        let small = ring_sizes(MIB);
        assert_eq!((small.region, small.window), (RING_MIN, RING_MIN));
        assert_eq!(ring_sizes(16 * MIB + 5).region % PAGE, 0);
        assert_eq!(ring_sizes(u64::MAX).region, WINDOW_MAX);
    }

    #[test]
    fn p_a_record_is_kept_in_place_while_a_window_stays_free() {
        assert!(keep_in_place(0, 2));
        assert!(!keep_in_place(1, 2));
        assert!(keep_in_place(2, 4));
        assert!(!keep_in_place(3, 4));
        assert!(!keep_in_place(0, 1));
    }

    #[test]
    fn p_an_image_declares_its_processes_in_its_environment() {
        let env = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(processes(&env(&[])), Ok(1));
        assert_eq!(processes(&env(&["MORUNA_PROCESSES=8"])), Ok(8));
        assert!(processes(&env(&["MORUNA_PROCESSES=0"])).is_err());
        assert!(processes(&env(&["MORUNA_PROCESSES=65"])).is_err());
        assert!(processes(&env(&["MORUNA_PROCESSES=x"])).is_err());
    }

    #[test]
    fn p_an_image_declares_the_rings_in_its_environment() {
        let env = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(declares_rings(&env(&["PATH=/bin", "MORUNA_PLANE=rings"])));
        assert!(!declares_rings(&env(&["MORUNA_PLANE=stdio"])));
        assert!(!declares_rings(&env(&[])));
    }

    #[test]
    fn kd2_t7_windows_are_whole_pages_within_their_bounds() {
        assert_eq!(window_for(0), WINDOW_MIN);
        assert_eq!(window_for(100), pages(200 + WINDOW_MIN).unwrap());
        assert_eq!(window_for(10 << 20), 21 << 20);
        assert_eq!(window_for(200 << 20), WINDOW_MAX);
        assert_eq!(window_for(u64::MAX), WINDOW_MAX);
        assert_eq!(window_for(5000) % PAGE, 0);
        assert_eq!(pages(1), Some(PAGE));
        assert_eq!(pages(PAGE), Some(PAGE));
        assert_eq!(pages(u64::MAX), None);
    }

    #[test]
    fn kd2_t7_a_record_includes_its_final_ipc_padding() {
        assert_eq!(record_len(4096, &[]), Some(4096));
        assert_eq!(record_len(4096, &[(4096, 80), (8192, 8)]), Some(8200));
        assert_eq!(
            record_len(8192, &[(4096, 8)]),
            Some(8192),
            "framing past the buffers"
        );
        assert_eq!(record_len(4096, &[(12288, 422)]), Some(12712));
        assert_eq!(record_len(0, &[(u64::MAX, 0)]), None);
        assert_eq!(record_len(0, &[(u64::MAX, 1)]), None);
    }

    #[test]
    fn kb5_t6_a_task_s_account_is_its_ceiling_within_half_the_engine_s_room() {
        const MIB: u64 = 1 << 20;
        // The ceiling, when the engine has room for twice it.
        assert_eq!(task_limit(256 * MIB, 1024 * MIB), Some(256 * MIB));
        // Half the room, when the ceiling is more.
        assert_eq!(task_limit(1024 * MIB, 600 * MIB), Some(300 * MIB));
        // Whole pages.
        assert_eq!(task_limit(u64::MAX, 100 * MIB + 3), Some(50 * MIB));
        assert_eq!(task_limit(20 * MIB + 5, 1 << 40), Some(20 * MIB));
        // Too little for a task.
        assert_eq!(task_limit(8 * MIB, 1 << 40), None);
        assert_eq!(task_limit(1 << 40, 2 * TASK_MIN - PAGE), None);
        assert_eq!(task_limit(1 << 40, 2 * TASK_MIN), Some(TASK_MIN));
    }

    #[test]
    fn kb5_t6_a_task_s_account_is_named_in_the_kernel_s_alphabet() {
        assert_eq!(account_name("transform"), "task.transform");
        assert_eq!(account_name("Tap 2/x"), "task.tap-2-x");
        let long = account_name(&"y".repeat(100));
        assert_eq!(long.len(), 32);
        assert!(long.starts_with("task.yyy"));
        assert_eq!(account_name(""), "task.");
    }

    #[test]
    fn kd2_t7_ports_are_numbered_from_one_in_name_order() {
        let names = vec!["audit".to_string(), "tap".to_string()];
        assert_eq!(port_name(&names, 1), Some("audit"));
        assert_eq!(port_name(&names, 2), Some("tap"));
        assert_eq!(port_name(&names, 0), None);
        assert_eq!(port_name(&names, 3), None);
        assert_eq!(line(b"x"), b"x\n");
    }
}
