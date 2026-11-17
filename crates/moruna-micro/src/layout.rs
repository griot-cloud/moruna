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

/// The most a native task's account is carved at (`abi: moruna`): what such a task holds is
/// its process, its image and its heap; its batches and records are in the engine's plane, not
/// in its account. A stage's whole share (the run's `child_share`) was carved for each task,
/// and the stage's outside memory counts every carved limit as held: eight Score tasks at
/// 20.5 GiB put 14.8 GiB on the engine's samples at stage start, near the controller's
/// memory trip; their accounts then peaked under 4 MB (the NYC gate, 2026-11-17).
pub const NATIVE_TASK_MAX: u64 = 128 << 20;

// Eight native tasks are counted at no more than 1 GiB together.
const _: () = assert!(8 * NATIVE_TASK_MAX <= 1 << 30);

/// A native task's plane budget and account ceiling, both out of its `share` of the stage's
/// budget: a quarter for its plane ([`ring_sizes`], at most what it holds in flight), and what
/// is left past the plane's real size for its account, at most [`NATIVE_TASK_MAX`]. The two
/// together never pass the share, so a native stage costs the run what the run set aside for
/// it: a plane of half the share beside an account of the whole one was refused at a 2 GiB
/// budget (the NYC gate, 2026-11-17).
pub fn native_split(share: u64) -> (u64, u64) {
    let budget = share / 4;
    let s = ring_sizes(budget);
    let plane = s.region + 2 * s.windows * s.window;
    let account = share.saturating_sub(plane).min(NATIVE_TASK_MAX) & !(PAGE - 1);
    (budget, account)
}

/// [`task_limit`] for a native task, out of its `share` ([`native_split`]).
pub fn native_task_limit(share: u64, room: u64) -> Option<u64> {
    task_limit(native_split(share).1, room)
}

/// The least share a native task runs in: its account at [`TASK_MIN`] beside its plane at its
/// least (pieces of [`RING_MIN`]).
pub const NATIVE_CHILD_MIN: u64 = TASK_MIN + PLANE_HALF_PIECES * RING_MIN / 2;

/// The CPU a stage's task `instance` is placed on, of `cpus`: one after the instance, so the
/// first CPU, where the engine's first worker deals the stage's batches, takes a task last.
pub fn task_cpu(instance: usize, cpus: usize) -> u32 {
    u32::try_from((instance + 1) % cpus.max(1)).unwrap_or(0)
}

/// What one task of a stage did, for the run's report: summed over every task the instance
/// had (one, unless the scheduler retired it and started another).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskFigures {
    /// Tasks the instance had.
    pub states: u64,
    /// Batches it was given.
    pub batches: u64,
    /// Records it put.
    pub records: u64,
    /// Answers the stage gave as batches of no rows, the answer coming later.
    pub deferred: u64,
    /// The CPU it was placed on.
    pub cpu: Option<u32>,
    /// Its account's peak, in bytes.
    pub peak: u64,
    /// Its account's limit, in bytes.
    pub limit: u64,
}

/// The run report's note of a ring stage's tasks: each one's batches, CPU and account, and
/// the most of them that held a batch at once (`busy_peak`). Tasks with no figures (not ended
/// as the stage ended) are said as such.
pub fn stage_note(stage: &str, tasks: &[Option<TaskFigures>], busy_peak: usize) -> String {
    let mut note = format!(
        "image `{stage}`: {} tasks, busy_peak {busy_peak} (the most holding a batch at once)",
        tasks.len()
    );
    for (i, t) in tasks.iter().enumerate() {
        match t {
            Some(t) => note.push_str(&format!(
                "; task {i}: {} batches, {} records, {} deferred, cpu {}, account peak {} of {} bytes{}",
                t.batches,
                t.records,
                t.deferred,
                t.cpu.map_or_else(|| "any".to_string(), |c| c.to_string()),
                t.peak,
                t.limit,
                if t.states > 1 {
                    format!(", {} tasks in turn", t.states)
                } else {
                    String::new()
                }
            )),
            None => note.push_str(&format!("; task {i}: no figures")),
        }
    }
    note
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

/// The sizes of a ring task's plane (SDD 30 f.6), from what it holds in flight: the largest
/// record the engine lays out for it (a piece: a bigger batch goes to the task cut into
/// pieces), [`IN_FLIGHT`] of them and one more in its input region, and [`WINDOWS`] windows of
/// a piece and a half each, for an answer up to half again its batch (a bigger one goes in
/// parts). The region, the rings object in use and
/// one handed back with its records still held fit the stage's plane budget together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingSizes {
    /// The largest record the engine lays out for the task.
    pub piece: u64,
    /// The input region's bytes.
    pub region: u64,
    /// The windows of a rings object.
    pub windows: u64,
    /// Each window's bytes.
    pub window: u64,
}

/// Batches each of a ring stage's tasks holds at once: one it works on while the engine lays
/// out the next, so one engine worker keeps every task of the stage busy.
pub const IN_FLIGHT: u64 = 3;

/// The windows of a ring task's rings object: one an answer in flight, so a task can answer a
/// morsel of [`IN_FLIGHT`] pieces between two of the stage's looks at it.
pub const WINDOWS: u64 = IN_FLIGHT;

/// A task's plane in half pieces: its region of [`IN_FLIGHT`] pieces and one more, and two
/// rings objects (the one in use and one handed back) of [`WINDOWS`] windows of a piece and a
/// half each.
const PLANE_HALF_PIECES: u64 = 2 * (IN_FLIGHT + 1) + 2 * WINDOWS * 3;

/// Pieces a task may have queued, given to the stage and not yet posted to it, before the
/// stage holds the worker that brings it more until it has worked some off: a morsel's pieces
/// past what a task holds in flight wait here, not the worker. A worker that waited for a
/// task's oldest answer before each piece past the second kept the other seven tasks waiting
/// for it (32 MiB morsels of three pieces, the NYC gate at 20.5 GiB, 2026-11-17).
pub const QUEUED_MAX: usize = 2 * IN_FLIGHT as usize;

/// How many of a task's `queued` pieces are posted to it now, with `in_flight` of its pieces
/// posted and not answered: as many as [`IN_FLIGHT`] leaves room for.
pub fn to_post(in_flight: usize, queued: usize) -> usize {
    (IN_FLIGHT as usize).saturating_sub(in_flight).min(queued)
}

/// Whether a task with `queued` pieces holds the worker that brought them, which waits for its
/// oldest answer before it goes on: only past [`QUEUED_MAX`].
pub fn holds_worker(queued: usize) -> bool {
    queued > QUEUED_MAX
}

/// The largest piece: the region and windows it makes are what a task holds in flight, never a
/// share of the budget. Budget shares made each task's plane up to 512 MiB, all of it zeroed
/// when it was made and charged to the engine (the NYC gate, 2026-11-17).
pub const PIECE_MAX: u64 = 16 << 20;

/// The least region and window: a record of a small batch and its framing.
pub const RING_MIN: u64 = 1 << 20;

/// The largest memory object the kernel makes (`moruna_abi::limits::MEMORY_MAX_BYTES`): a
/// task's region is one, and its rings object, every window and the rings' pages, is another.
pub const OBJECT_MAX: u64 = 256 << 20;

/// What a rings object holds before its windows, at most: its header and its rings' entries
/// (`moruna_plane::ring::Geometry::windows_at`, a few pages for two windows).
pub const RINGS_HEAD_MAX: u64 = 1 << 20;

/// [`RingSizes`] for a plane of `budget` bytes, in whole pages. Whatever the budget, the region
/// and the rings object are each one object the kernel makes: the windows are bounded so the
/// rings object, its head included, is at most [`OBJECT_MAX`]. Two windows of an eighth of a
/// 1 GiB budget were 256 MiB with the head past it, and the kernel refused the object
/// (`Kernel(Limit)` opening the task's plane, the NYC gate at 8 GiB and more, 2026-11-17).
pub fn ring_sizes(budget: u64) -> RingSizes {
    let piece =
        ((budget / PLANE_HALF_PIECES).saturating_mul(2) & !(PAGE - 1)).clamp(RING_MIN, PIECE_MAX);
    RingSizes {
        piece,
        region: (IN_FLIGHT + 1) * piece,
        windows: WINDOWS,
        window: (piece + piece / 2) & !(PAGE - 1),
    }
}

// The largest plane is well within the kernel's largest object.
const _: () = assert!((IN_FLIGHT + 1) * PIECE_MAX <= OBJECT_MAX);
const _: () = assert!(WINDOWS * (PIECE_MAX + PIECE_MAX / 2) + RINGS_HEAD_MAX <= OBJECT_MAX);

/// The most of a stage's budget one native task stands for (the run's `child_share`): the share
/// whose quarter makes its plane at its largest ([`native_split`]); its account takes at most
/// [`NATIVE_TASK_MAX`] of the rest.
pub const NATIVE_CHILD_MAX: u64 = 4 * PLANE_HALF_PIECES * PIECE_MAX / 2;

// A native task stands for less than a gibibyte of its stage's budget.
const _: () = assert!(NATIVE_CHILD_MAX < 1 << 30);

/// Rows of a batch of `rows` rows whose record is `len` bytes cut into pieces of at most
/// `piece` bytes, by its bytes a row, a tenth spare for the framing and the pages: each piece's
/// first row and row count, in order. One piece when it fits, or when it has one row.
pub fn cut(rows: usize, len: u64, piece: u64) -> Vec<(usize, usize)> {
    if len <= piece || rows <= 1 || piece == 0 {
        return vec![(0, rows)];
    }
    let target = piece - piece / 10;
    let count = usize::try_from(len.div_ceil(target.max(1)))
        .unwrap_or(rows)
        .clamp(1, rows);
    let each = rows.div_ceil(count);
    (0..rows)
        .step_by(each)
        .map(|at| (at, each.min(rows - at)))
        .collect()
}

/// Values made ahead, each on a thread of its own, and taken in turn: what a stage makes for
/// each of its tasks before the task starts, so the kernel's zeroing of it and the host's
/// faulting it in are spread over the CPUs, not paid one task after another. A native stage's
/// eight planes, made each as its task started, held its start about 3 s under nested KVM
/// (0.9 GiB, the NYC gate, 2026-11-17).
pub struct Ahead<T> {
    made: std::sync::Mutex<Vec<Option<std::thread::JoinHandle<T>>>>,
}

impl<T: Send + 'static> Default for Ahead<T> {
    fn default() -> Self {
        Ahead {
            made: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl<T: Send + 'static> Ahead<T> {
    /// Starts making the values of `0..n`, `make(i)` each on a thread of its own, unless they
    /// were started already.
    pub fn start(&self, n: usize, make: impl Fn(usize) -> T + Send + Sync + 'static) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if !made.is_empty() {
            return;
        }
        let make = std::sync::Arc::new(make);
        *made = (0..n)
            .map(|i| {
                let make = std::sync::Arc::clone(&make);
                std::thread::Builder::new()
                    .name(format!("ahead-{i}"))
                    .spawn(move || make(i))
                    .ok()
            })
            .collect();
    }

    /// The value of `i`, waited for; `None` when it was not made ahead (never started, taken
    /// already, or its thread could not start or panicked): the caller makes it now.
    pub fn take(&self, i: usize) -> Option<T> {
        let handle = self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(i)
            .and_then(Option::take)?;
        handle.join().ok()
    }
}

/// A stage's cleanup of its task's account (SB-I9): the task's `process` handle, for a native
/// task the stage started itself, is released first, since the process it names (its address
/// space, its image) is charged to the account; then `usage` (bytes used and reserved, `None`
/// when the account is gone) is read until both are zero, for at most `wait`. Holding the
/// handle while waiting made every native task's account look busy for the whole wait: eight
/// tasks held a finished run 40 s (the NYC gate, 2026-11-17).
pub fn retire<P>(
    process: Option<P>,
    mut usage: impl FnMut() -> Result<Option<(u64, u64)>, String>,
    wait: std::time::Duration,
) -> Result<(), String> {
    drop(process);
    let deadline = std::time::Instant::now() + wait;
    loop {
        match usage()? {
            Some((0, 0)) => return Ok(()),
            None => return Err("original Account observer disappeared before retirement".into()),
            Some(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err("actual child Account did not retire within bounded cleanup".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
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
    fn p_ring_sizes_are_what_a_task_holds_in_flight() {
        const MIB: u64 = 1 << 20;
        // A big budget: the largest piece, three of them in the region, windows of two.
        let s = ring_sizes(1 << 30);
        assert_eq!(
            s,
            RingSizes {
                piece: PIECE_MAX,
                region: 4 * PIECE_MAX,
                windows: 3,
                window: 3 * PIECE_MAX / 2,
            }
        );
        assert_eq!(ring_sizes(u64::MAX), s, "never more, whatever the budget");
        // In use: the region and one rings object, 136 MiB at most, where 512 MiB were.
        assert_eq!(s.region + s.windows * s.window, 136 * MIB);
        // A small budget: the region, the rings object and one handed back fit it.
        for budget in [16 * MIB, 64 * MIB, 100 * MIB + 5, 150 * MIB] {
            let s = ring_sizes(budget);
            assert!(
                s.region + 2 * s.windows * s.window <= budget,
                "{budget}: {s:?}"
            );
            assert_eq!(
                (s.piece % PAGE, s.window % PAGE, s.region),
                (0, 0, (IN_FLIGHT + 1) * s.piece)
            );
            assert!(s.window + PAGE > s.piece + s.piece / 2);
        }
        let small = ring_sizes(MIB);
        assert_eq!((small.piece, small.region), (RING_MIN, 4 * RING_MIN));
    }

    /// The stage's worker dealing morsels of `pieces` pieces to `tasks` tasks in turn, `cost`
    /// ticks of its own a morsel (its decode, layout and encode), each piece `work` ticks of a
    /// task's: when the last task ends, and the ticks the worker spent waiting on one task.
    /// `queue` is this layout's policy; without it the worker waits for a task's oldest answer
    /// before each piece past what it holds in flight.
    fn deal(
        tasks: usize,
        morsels: usize,
        pieces: usize,
        (cost, work): (u64, u64),
        queue: bool,
    ) -> (u64, u64) {
        let mut now = 0u64;
        let mut waited = 0u64;
        // Each task: when its posted pieces end, oldest first, and its queued pieces.
        let mut posted: Vec<std::collections::VecDeque<u64>> = vec![Default::default(); tasks];
        let mut queued = vec![0usize; tasks];
        let mut free = vec![0u64; tasks];
        let post = |t: usize,
                    now: u64,
                    posted: &mut Vec<std::collections::VecDeque<u64>>,
                    free: &mut Vec<u64>| {
            let start = now.max(free[t]);
            free[t] = start + work;
            posted[t].push_back(free[t]);
        };
        for m in 0..morsels {
            let t = m % tasks;
            now += cost;
            let done = |posted: &mut std::collections::VecDeque<u64>, now: u64| {
                while posted.front().is_some_and(|&end| end <= now) {
                    posted.pop_front();
                }
            };
            done(&mut posted[t], now);
            if queue {
                queued[t] += pieces;
                loop {
                    done(&mut posted[t], now);
                    for _ in 0..to_post(posted[t].len(), queued[t]) {
                        post(t, now, &mut posted, &mut free);
                        queued[t] -= 1;
                    }
                    if !holds_worker(queued[t]) {
                        break;
                    }
                    let end = posted[t][0];
                    waited += end - now;
                    now = end;
                }
            } else {
                for _ in 0..pieces {
                    done(&mut posted[t], now);
                    if posted[t].len() >= IN_FLIGHT as usize {
                        let end = posted[t][0];
                        waited += end - now;
                        now = end;
                        done(&mut posted[t], now);
                    }
                    post(t, now, &mut posted, &mut free);
                }
            }
        }
        // The stage's end: every queued piece goes, each as its task has room.
        for t in 0..tasks {
            while queued[t] > 0 {
                let start = now.max(free[t]);
                free[t] = start + work;
                queued[t] -= 1;
            }
        }
        (free.into_iter().max().unwrap_or(0), waited)
    }

    #[test]
    fn p_a_morsel_s_pieces_past_a_task_s_room_wait_in_its_queue_not_the_worker() {
        // The gate's early morsels: eight tasks, morsels of three pieces, the worker's own
        // work on a morsel about one piece's work of a task's (a 32 MiB morsel's decode, layout
        // and encode beside a 16 MiB piece of Score).
        // A morsel of the pieces a task holds in flight: the worker never waits.
        assert_eq!(deal(8, 64, IN_FLIGHT as usize, (10, 10), true).1, 0);
        // More pieces than that: the extra wait in the task's queue, not the worker.
        let pieces = IN_FLIGHT as usize + 1;
        let (old_end, old_waited) = deal(8, 64, pieces, (10, 10), false);
        let (end, waited) = deal(8, 64, pieces, (10, 10), true);
        assert!(
            waited * 2 <= old_waited,
            "waited {waited}, was {old_waited}"
        );
        assert!(end * 10 < old_end * 9, "ended at {end}, was {old_end}");
        // Tasks slower than the worker deals: the queue's bound holds it, as it must.
        assert!(deal(8, 64, 3, (1, 20), true).1 > 0);
        // The queue is bounded: a task far behind holds the worker again.
        assert!(!holds_worker(QUEUED_MAX));
        assert!(holds_worker(QUEUED_MAX + 1));
        assert_eq!(to_post(0, 5), IN_FLIGHT as usize);
        assert_eq!(to_post(IN_FLIGHT as usize, 5), 0);
        assert_eq!(to_post(1, 0), 0);
        // One task, all the work: the queue bounds what waits, and nothing is lost.
        let (one, _) = deal(1, 10, 3, (1, 5), true);
        assert_eq!(one, deal(1, 10, 3, (1, 5), false).0);
    }

    #[test]
    fn p_a_batch_past_a_piece_is_cut_into_pieces_in_order() {
        const MIB: u64 = 1 << 20;
        assert_eq!(cut(1000, 10 * MIB, 16 * MIB), [(0, 1000)], "fits");
        assert_eq!(
            cut(1, 100 * MIB, 16 * MIB),
            [(0, 1)],
            "one row is one piece"
        );
        let pieces = cut(1_000_000, 100 * MIB, 16 * MIB);
        assert!(pieces.len() >= 7, "{}", pieces.len());
        // Every row once, in order, and each piece's share of the bytes under a piece.
        let mut next = 0;
        for &(at, rows) in &pieces {
            assert_eq!(at, next);
            assert!(rows > 0);
            assert!((100 * MIB) * rows as u64 / 1_000_000 <= 16 * MIB - 16 * MIB / 10 + MIB);
            next = at + rows;
        }
        assert_eq!(next, 1_000_000);
        // Fewer rows than pieces: one row each at most.
        assert_eq!(cut(3, 100 * MIB, 16 * MIB), [(0, 1), (1, 1), (2, 1)]);
    }

    #[test]
    fn p_a_task_s_objects_fit_the_kernel_s_largest_at_any_budget() {
        const MIB: u64 = 1 << 20;
        // From the least plane to far past any node, and around where the gate was refused.
        let budgets = (0..=46).map(|shift| 1u64 << shift).chain([
            1020 * MIB,
            1 << 30,
            (1 << 30) + PAGE,
            3 << 30,
            20_992 * MIB,
            u64::MAX,
        ]);
        for budget in budgets {
            let s = ring_sizes(budget);
            assert!(s.region <= OBJECT_MAX, "region at {budget}: {s:?}");
            assert!(
                s.windows * s.window + RINGS_HEAD_MAX <= OBJECT_MAX,
                "rings object at {budget}: {s:?}"
            );
            assert!(s.window >= RING_MIN && s.region >= RING_MIN);
            assert_eq!((s.region % PAGE, s.window % PAGE), (0, 0));
        }
    }

    #[test]
    fn p_a_native_task_s_plane_and_account_fit_its_share() {
        const MIB: u64 = 1 << 20;
        let mut share = NATIVE_CHILD_MIN;
        while share <= 64 << 30 {
            let (budget, account) = native_split(share);
            let s = ring_sizes(budget);
            let plane = s.region + 2 * s.windows * s.window;
            assert!(
                plane + account <= share,
                "{share}: plane {plane}, account {account}"
            );
            assert!(account >= TASK_MIN, "{share}: account {account}");
            assert!(account <= NATIVE_TASK_MAX);
            assert_eq!(native_task_limit(share, 64 << 30), Some(account));
            share = share * 5 / 4 + PAGE;
        }
        // At the most a native task stands for, its plane is at its largest.
        let (budget, account) = native_split(NATIVE_CHILD_MAX);
        assert_eq!(ring_sizes(budget).piece, PIECE_MAX);
        assert_eq!(account, NATIVE_TASK_MAX);
        // Under the least, the account does not fit.
        assert_eq!(
            native_task_limit(NATIVE_CHILD_MIN - 2 * MIB, 64 << 30),
            None
        );
        // Less room in the engine's account is what it is.
        assert_eq!(
            native_task_limit(NATIVE_CHILD_MAX, 100 * MIB),
            Some(50 * MIB)
        );
    }

    #[test]
    fn p_a_stage_s_tasks_go_one_to_a_cpu_the_dealer_s_last() {
        let cpus: Vec<u32> = (0..8).map(|i| task_cpu(i, 8)).collect();
        assert_eq!(cpus, [1, 2, 3, 4, 5, 6, 7, 0]);
        assert_eq!(task_cpu(8, 8), 1, "more tasks than CPUs go round again");
        assert_eq!(task_cpu(0, 1), 0);
        assert_eq!(task_cpu(3, 0), 0, "no CPU count: the first");
    }

    #[test]
    fn p_the_report_says_each_task_s_batches_cpu_and_account() {
        let t = TaskFigures {
            states: 1,
            batches: 12,
            records: 13,
            deferred: 2,
            cpu: Some(3),
            peak: 40 << 20,
            limit: 128 << 20,
        };
        let note = stage_note("score", &[Some(t), None, Some(TaskFigures::default())], 2);
        assert!(
            note.starts_with("image `score`: 3 tasks, busy_peak 2"),
            "{note}"
        );
        assert!(
            note.contains(
                "task 0: 12 batches, 13 records, 2 deferred, cpu 3, account peak 41943040 of 134217728 bytes;"
            ),
            "{note}"
        );
        assert!(note.contains("task 1: no figures"), "{note}");
        let again = TaskFigures { states: 2, ..t };
        assert!(
            stage_note("score", &[Some(again)], 1).ends_with(", 2 tasks in turn"),
            "a retired instance's task and the next are both said"
        );
        assert!(
            note.contains("task 2: 0 batches, 0 records, 0 deferred, cpu any"),
            "{note}"
        );
    }

    #[test]
    fn p_a_stage_s_planes_are_made_ahead_together_and_taken_in_turn() {
        use std::time::{Duration, Instant};
        let ahead = Ahead::<usize>::default();
        assert_eq!(ahead.take(0), None, "nothing made ahead yet");
        let started = Instant::now();
        ahead.start(8, |i| {
            std::thread::sleep(Duration::from_millis(100));
            i * 10
        });
        // A second start makes nothing more.
        ahead.start(8, |_| unreachable!("made twice"));
        let taken: Vec<_> = (0..8).map(|i| ahead.take(i)).collect();
        let took = started.elapsed();
        assert_eq!(taken, (0..8).map(|i| Some(i * 10)).collect::<Vec<_>>());
        assert!(
            took < Duration::from_millis(500),
            "eight made together, not one after another: {took:?}"
        );
        assert_eq!(ahead.take(3), None, "each taken once");
        assert_eq!(ahead.take(8), None, "none past those made");
        // A value whose making panicked is made by the caller.
        let failing = Ahead::<u8>::default();
        failing.start(1, |_| panic!("its plane could not be made"));
        assert_eq!(failing.take(0), None);
    }

    #[test]
    fn p_a_native_task_s_process_goes_before_its_account_is_asked_to_retire() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        // The process handle: while it lives, its process is charged to the account.
        struct Handle(Arc<AtomicBool>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let gone = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&gone);
        let usage = move || {
            Ok(Some(if seen.load(Ordering::SeqCst) {
                (0, 0)
            } else {
                (8 << 20, 0)
            }))
        };
        let started = Instant::now();
        retire(
            Some(Handle(Arc::clone(&gone))),
            usage,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "retired at once, not at the bound: {:?}",
            started.elapsed()
        );
        // A task of the personality has no handle here; its account is read as it is.
        retire(None::<Handle>, || Ok(Some((0, 0))), Duration::ZERO).unwrap();
    }

    #[test]
    fn p_an_account_that_does_not_retire_or_is_gone_is_said() {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let busy = retire(
            None::<()>,
            || Ok(Some((4096, 4096))),
            Duration::from_millis(20),
        );
        assert!(busy.unwrap_err().contains("did not retire"));
        assert!(started.elapsed() >= Duration::from_millis(20));
        let gone = retire(None::<()>, || Ok(None), Duration::from_secs(5));
        assert!(gone.unwrap_err().contains("disappeared"));
        let unread = retire(
            None::<()>,
            || Err("actual Account read: BadHandle".into()),
            Duration::from_secs(5),
        );
        assert_eq!(unread.unwrap_err(), "actual Account read: BadHandle");
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
