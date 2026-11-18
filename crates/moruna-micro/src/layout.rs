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

/// The largest piece one task of a stage takes (system design 3A; PM, 2026-11-18): its plane's
/// piece, or `account`, the piece its account holds by the function's measured line, where that
/// is less; `account` is 0 when nothing measured the function. A batch is cut at it, so none is
/// past what its task's account holds, nor its plane's input region.
pub fn piece_cap(plane: u64, account: u64) -> u64 {
    if account == 0 {
        plane
    } else {
        plane.min(account)
    }
}

/// [`task_limit`] for a native task, out of its `share` ([`native_split`]).
pub fn native_task_limit(share: u64, room: u64) -> Option<u64> {
    task_limit(native_split(share).1, room)
}

/// The pieces a measured function's instances are sized for, the largest first (system design
/// 3A): from [`PIECE_MAX`] down to [`RING_MIN`], halving. When the job's instances do not all
/// fit, the piece shrinks first, down to the least, and only then do fewer instances run (PM,
/// 2026-11-18): memory bounds the unit of work, never the workers.
pub const PIECES: [u64; 5] = [
    PIECE_MAX,
    PIECE_MAX / 2,
    PIECE_MAX / 4,
    PIECE_MAX / 8,
    RING_MIN,
];

const _: () = assert!(PIECE_MAX / 16 == RING_MIN);

/// The plane a task gets for pieces of `piece` bytes: the plane budget that makes its pieces
/// that size ([`ring_sizes`]) and the bytes the plane then is (its region and its two rings
/// objects of windows).
pub fn plane_for(piece: u64) -> (u64, u64) {
    let budget = piece.clamp(RING_MIN, PIECE_MAX) / 2 * PLANE_HALF_PIECES;
    let s = ring_sizes(budget);
    (budget, s.region + 2 * s.windows * s.window)
}

/// The account a measured task is carved at (system design 3A): `account` bytes, what the
/// node's readings of the function say one of its processes holds at its piece, in whole pages,
/// and never more than half of what is left in the engine's account (`room`), as
/// [`task_limit`] keeps it. The least [`TASK_MIN`] is not applied: it is where a function with
/// no measurement starts, and a measured function gets its measured need (PM, 2026-11-18).
/// `None` when not a page is left.
pub fn measured_task_limit(account: u64, room: u64) -> Option<u64> {
    let limit = account.next_multiple_of(PAGE).min(room / 2) & !(PAGE - 1);
    (limit >= PAGE).then_some(limit)
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
    /// The largest piece it was given: the bytes of the batch's buffers, without the record's
    /// framing. With `peak`, the pair a run refines the function's measured memory from
    /// (system design 3A).
    pub largest: u64,
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
                "; task {i}: {} batches, {} records, {} deferred, cpu {}, account peak {} of {} bytes, largest piece {} bytes{}",
                t.batches,
                t.records,
                t.deferred,
                t.cpu.map_or_else(|| "any".to_string(), |c| c.to_string()),
                t.peak,
                t.limit,
                t.largest,
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

/// Pieces a task may keep queued, given to the stage and not yet posted to it, once the unit
/// that brought them returns: none. A queued piece is a slice of its morsel and holds the
/// morsel's memory, and a queue is fed only by the stage's next unit for that task. A run under
/// memory pressure runs no unit until memory frees, so pieces left queued held the memory that
/// would let the next unit run: one-month Score at a 2 GiB budget stopped with 30% of its rows
/// in the tasks' queues, 680 s, bottleneck Memory (the NYC gate, 2026-11-17). A unit therefore
/// posts its morsel's pieces as its task makes room, taking answers meanwhile, before it
/// returns; a morsel of [`IN_FLIGHT`] pieces, which the sink's 64 MiB lanes keep the gate's
/// to, posts at once and never waits.
pub const QUEUED_MAX: usize = 0;

/// How many of a task's `queued` pieces are posted to it now, with `in_flight` of its pieces
/// posted and not answered: as many as [`IN_FLIGHT`] leaves room for.
pub fn to_post(in_flight: usize, queued: usize) -> usize {
    (IN_FLIGHT as usize).saturating_sub(in_flight).min(queued)
}

/// Whether a task with `queued` pieces holds the worker that brought them, which waits for its
/// oldest answer and posts another before it goes on: past [`QUEUED_MAX`].
pub fn holds_worker(queued: usize) -> bool {
    queued > QUEUED_MAX
}

/// Whether a unit answers its whole morsel before it returns: the stage's first, the one that
/// begins before any of the stage's tasks has answered, so before its answers' schema is known.
///
/// The controller measures a stage with a probe, one morsel through one unit, and sizes the
/// stage from what that unit took and gave: its bytes out over its bytes in, its time and its
/// peak. The probe is the stage's first unit. A unit that returns with the answers that have
/// come gives the answers to `max(1, n - IN_FLIGHT)` of its morsel's `n` pieces, so the
/// controller read a part of a function's output as the whole: half, for 6.2 run 3's 16 MiB
/// probe in pieces of 16 MiB (6.3j). Every later unit returns with what has come, as before:
/// one engine worker keeps every task of the stage busy.
pub fn answers_whole(schema_known: bool) -> bool {
    !schema_known
}

/// Whether a unit waits for its task's oldest answer before it goes on: while it has pieces
/// queued ([`holds_worker`]), and, when it answers its whole morsel ([`answers_whole`]), while
/// any piece is in flight.
pub fn waits_for_answer(queued: usize, in_flight: usize, whole: bool) -> bool {
    holds_worker(queued) || (whole && in_flight > 0)
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

/// How long a stage waits on its task with no answer come before it says where the wait stands,
/// and again each time as long again: a wait that never ended used to say nothing (item 3.1,
/// the full-year hang). A log line, not a cancel.
pub const WAIT_NOTE_NS: u64 = 60 * 1_000_000_000;

/// When a wait on a task is due to say where it stands ([`WAIT_NOTE_NS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitNotes {
    since: u64,
    next: u64,
}

impl WaitNotes {
    /// A wait begun at `now`, in nanoseconds of the kernel's clock.
    pub fn begin(now: u64) -> WaitNotes {
        WaitNotes {
            since: now,
            next: now.saturating_add(WAIT_NOTE_NS),
        }
    }

    /// At `now`: the whole seconds the wait has lasted when it is due to say so, the next
    /// time [`WAIT_NOTE_NS`] later; `None` until then.
    pub fn due(&mut self, now: u64) -> Option<u64> {
        if now < self.next {
            return None;
        }
        self.next = now.saturating_add(WAIT_NOTE_NS);
        Some(now.saturating_sub(self.since) / 1_000_000_000)
    }
}

/// The line a stage writes while it waits on its task with no answer come: the stage, the
/// task (its instance), how long, what the stage holds of the task's, and each of the task's
/// windows as the plane has it (`given`, `held`, `idle`, `fresh`).
pub fn wait_note(
    stage: &str,
    task: usize,
    waited_s: u64,
    (in_flight, queued, held): (usize, usize, u64),
    windows: Option<&[&str]>,
) -> String {
    format!(
        "image `{stage}` task {task}: waited {waited_s} s for its answer with none come; \
         {in_flight} batches in flight, {queued} pieces queued, {held} records kept in place; \
         its windows: {}",
        windows.map_or_else(|| String::from("not a ring task's"), |w| w.join(", "))
    )
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

/// Where the user target's linker script (`kernel/sys/user.ld`) puts a native program's first
/// segment, its text.
pub const NATIVE_BASE: u64 = 0x40_0000;

const PT_LOAD: u32 = 1;
/// A note segment's program header: a native function's `.note.moruna` (system design 3E).
const PT_NOTE: u32 = 4;
/// A thread-local storage template's program header.
const PT_TLS: u32 = 7;
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
/// script, whose program headers are its loaded segments, read and execute first at 0x400000,
/// then read only, then read and write, at most one thread-local storage template, read only, as
/// a program with std has (`kernel/targets/x86_64-moruna.ld`, item 5.0), and at most one note
/// segment, read only, where a function declares its ceiling (`.note.moruna`, system design 3E),
/// which the kernel's loader takes. A Linux program has a stack header, so none passes. `Err`
/// says what does not match.
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
    if size != 56 || !(1..=5).contains(&count) {
        return Err(format!(
            "it has {count} program headers of {size} bytes, where the user target's linker \
             script writes one to three of 56 (its loaded segments), its note and, with std, \
             its thread-local storage"
        ));
    }
    let mut flags = Vec::new();
    let mut tls = false;
    let mut note = false;
    for i in 0..u64::from(count) {
        let at = usize::try_from(phoff + i * 56)
            .map_err(|_| String::from("its program headers lie past its bytes"))?;
        let ty = u32_at(elf, at)
            .ok_or_else(|| String::from("its program headers lie past its bytes"))?;
        // std's thread-local storage template (item 5.0): one, read only, after the first
        // segment.
        if ty == PT_TLS && i > 0 && !tls && u32_at(elf, at + 4).unwrap_or(0) & (PF_W | PF_X) == 0 {
            tls = true;
            continue;
        }
        // Its note segment (6.3e): one, read only, after the first segment.
        if ty == PT_NOTE && i > 0 && !note && u32_at(elf, at + 4).unwrap_or(0) & (PF_W | PF_X) == 0
        {
            note = true;
            continue;
        }
        if ty != PT_LOAD {
            return Err(format!(
                "its program header {i} is of type {ty:#x}, not a loaded segment: the user \
                 target's linker script writes none other (a Linux program has a stack header)"
            ));
        }
        let f = u32_at(elf, at + 4).unwrap_or(0);
        let vaddr = u64_at(elf, at + 16).unwrap_or(0);
        if flags.is_empty() && vaddr != NATIVE_BASE {
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
        // A program with std (item 5.0): its thread-local storage template, read only, beside
        // its segments; one only, and never first or writable or alone.
        let tls = (7, 4, NATIVE_BASE + 0x20000);
        assert_eq!(
            native_elf(&elf(2, 62, &[user[0], user[1], user[2], tls])),
            Ok(())
        );
        assert_eq!(native_elf(&elf(2, 62, &[user[0], tls, user[2]])), Ok(()));
        refused(elf(2, 62, &[user[0], tls, tls]), "type 0x7");
        refused(elf(2, 62, &[tls, user[0]]), "type 0x7");
        refused(
            elf(2, 62, &[user[0], (7, 6, NATIVE_BASE + 0x20000)]),
            "type 0x7",
        );
        refused(
            elf(2, 62, &[user[0], user[1], user[2], tls, tls]),
            "type 0x7",
        );
        // A function's note segment (6.3e, `.note.moruna`): one, read only, beside its segments
        // and its thread-local storage; never first, writable or a second.
        let note = (4, 4, 0);
        assert_eq!(
            native_elf(&elf(2, 62, &[user[0], user[1], user[2], tls, note])),
            Ok(())
        );
        assert_eq!(native_elf(&elf(2, 62, &[user[0], note])), Ok(()));
        refused(elf(2, 62, &[note, user[0]]), "type 0x4");
        refused(elf(2, 62, &[user[0], note, note]), "type 0x4");
        refused(elf(2, 62, &[user[0], (4, 6, 0)]), "type 0x4");
        refused(
            elf(2, 62, &[user[0], user[1], user[2], tls, note, note]),
            "6 program headers",
        );
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
    ) -> (u64, u64, usize) {
        let mut now = 0u64;
        let mut waited = 0u64;
        let mut left = 0usize;
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
                left = left.max(queued[t]);
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
        (free.into_iter().max().unwrap_or(0), waited, left)
    }

    #[test]
    fn p_a_morsel_s_pieces_past_a_task_s_room_wait_in_its_queue_not_the_worker() {
        // The gate's early morsels: eight tasks, morsels of three pieces, the worker's own
        // work on a morsel about one piece's work of a task's (a 32 MiB morsel's decode, layout
        // and encode beside a 16 MiB piece of Score).
        // A morsel of the pieces a task holds in flight: the worker never waits.
        assert_eq!(deal(8, 64, IN_FLIGHT as usize, (10, 10), true).1, 0);
        // More pieces than that: the worker posts them as its task makes room, and waits no
        // longer than it did for each piece.
        let pieces = IN_FLIGHT as usize + 1;
        let (old_end, old_waited, _) = deal(8, 64, pieces, (10, 10), false);
        let (end, waited, _) = deal(8, 64, pieces, (10, 10), true);
        assert!(waited <= old_waited, "waited {waited}, was {old_waited}");
        assert!(end <= old_end, "ended at {end}, was {old_end}");
        assert!(holds_worker(1) && !holds_worker(0));
        assert_eq!(to_post(0, 5), IN_FLIGHT as usize);
        assert_eq!(to_post(IN_FLIGHT as usize, 5), 0);
        assert_eq!(to_post(1, 0), 0);
        // One task, all the work: nothing is lost.
        let (one, _, _) = deal(1, 10, 3, (1, 5), true);
        assert_eq!(one, deal(1, 10, 3, (1, 5), false).0);
    }

    /// How many of a morsel's `pieces` its unit answers before it returns, over a task that
    /// answers a piece when it is waited for and not before: `TaskState::pipelined`'s steps.
    /// `schema_known` is whether any of the stage's tasks has answered before this unit.
    fn answered(pieces: usize, schema_known: bool) -> usize {
        let whole = answers_whole(schema_known);
        let (mut queued, mut in_flight, mut answered) = (pieces, 0usize, 0usize);
        let feed = |queued: &mut usize, in_flight: &mut usize| {
            let post = to_post(*in_flight, *queued);
            *queued -= post;
            *in_flight += post;
        };
        feed(&mut queued, &mut in_flight);
        if !schema_known {
            // The task's first answer, whose schema stands for the rest.
            in_flight -= 1;
            answered += 1;
            feed(&mut queued, &mut in_flight);
        }
        while waits_for_answer(queued, in_flight, whole) {
            in_flight -= 1;
            answered += 1;
            feed(&mut queued, &mut in_flight);
        }
        assert_eq!(queued, 0, "a unit leaves no piece queued");
        answered
    }

    #[test]
    fn p_the_stage_s_first_unit_answers_its_whole_morsel() {
        assert!(answers_whole(false) && !answers_whole(true));
        assert!(waits_for_answer(0, 1, true) && !waits_for_answer(0, 0, true));
        assert!(waits_for_answer(1, 0, false) && !waits_for_answer(0, 3, false));
        // The stage's first unit, which a probe is: every piece's answer, however many pieces
        // its morsel is. It gave the answers to max(1, n - IN_FLIGHT) of n: half of 6.2 run
        // 3's probe, a morsel of two pieces.
        for pieces in 1..=12 {
            assert_eq!(answered(pieces, false), pieces, "{pieces} pieces");
        }
        // Every later unit returns with what has come: none here, where the task answers only
        // what is waited for, but the answers it takes to post its queued pieces.
        for pieces in 1..=12 {
            assert_eq!(
                answered(pieces, true),
                pieces.saturating_sub(IN_FLIGHT as usize),
                "{pieces} pieces"
            );
        }
    }

    #[test]
    fn p_a_unit_leaves_no_piece_queued_so_a_stage_the_run_stops_visiting_holds_no_input() {
        // Whatever the morsels, the tasks and their pace, a unit returns with its task's queue
        // empty: a run that runs no unit under memory pressure finds no stage holding morsels
        // it cannot post (the drain that stopped one-month Score at 2 GiB for 680 s).
        for pieces in 1..=3 * IN_FLIGHT as usize {
            for (cost, work) in [(10, 10), (1, 20), (20, 1)] {
                for tasks in [1, 3, 8] {
                    let (_, _, left) = deal(tasks, 48, pieces, (cost, work), true);
                    assert_eq!(left, 0, "{pieces} pieces, {tasks} tasks, ({cost}, {work})");
                }
            }
        }
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
    fn a_task_s_piece_is_its_plane_s_or_less_where_its_account_holds_less() {
        let plane = ring_sizes(native_split(256 << 20).0).piece;
        assert_eq!(
            piece_cap(plane, 0),
            plane,
            "nothing measured: the plane's piece"
        );
        assert_eq!(
            piece_cap(plane, plane / 4),
            plane / 4,
            "the account holds less"
        );
        assert_eq!(
            piece_cap(plane, plane * 4),
            plane,
            "never past the input region"
        );
    }

    #[test]
    fn a_measured_task_s_plane_is_for_its_piece_and_its_account_its_need() {
        for piece in PIECES {
            let (budget, bytes) = plane_for(piece);
            let s = ring_sizes(budget);
            assert_eq!(s.piece, piece, "the plane is for pieces of {piece} bytes");
            assert_eq!(bytes, s.region + 2 * s.windows * s.window);
            assert!(bytes <= OBJECT_MAX * 2);
        }
        // Out of the pieces' range, the nearest end.
        assert_eq!(plane_for(1), plane_for(RING_MIN));
        assert_eq!(plane_for(u64::MAX), plane_for(PIECE_MAX));
        // Pieces of 16 MiB: 208 MiB of plane, as a share of 832 MiB makes.
        assert_eq!(plane_for(PIECE_MAX).1, NATIVE_CHILD_MAX / 4);
        // The account in whole pages, under half the room, and no least.
        assert_eq!(measured_task_limit(4_758_480, 1 << 30), Some(4_759_552));
        assert_eq!(measured_task_limit(64 << 20, 64 << 20), Some(32 << 20));
        assert_eq!(measured_task_limit(1, 1 << 30), Some(PAGE));
        assert_eq!(measured_task_limit(1 << 20, PAGE), None);
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
            largest: 16 << 20,
        };
        let note = stage_note("score", &[Some(t), None, Some(TaskFigures::default())], 2);
        assert!(
            note.starts_with("image `score`: 3 tasks, busy_peak 2"),
            "{note}"
        );
        assert!(
            note.contains(
                "task 0: 12 batches, 13 records, 2 deferred, cpu 3, account peak 41943040 of 134217728 bytes, largest piece 16777216 bytes;"
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
    fn p_3_1_a_long_wait_on_a_task_says_where_it_stands_every_minute() {
        const S: u64 = 1_000_000_000;
        let mut notes = WaitNotes::begin(5 * S);
        assert_eq!(notes.due(5 * S), None);
        assert_eq!(notes.due(64 * S), None, "under a minute");
        assert_eq!(notes.due(65 * S), Some(60), "a minute in");
        assert_eq!(notes.due(100 * S), None, "said once");
        assert_eq!(
            notes.due(125 * S + 3),
            Some(120),
            "and again a minute later"
        );
        assert_eq!(notes.due(126 * S), None);
        // A wait looked at late says it once, with how long it has really lasted.
        let mut late = WaitNotes::begin(0);
        assert_eq!(late.due(1000 * S), Some(1000));
        assert_eq!(late.due(1001 * S), None);
        let line = wait_note("score", 3, 120, (2, 1, 2), Some(&["held", "held", "idle"]));
        assert_eq!(
            line,
            "image `score` task 3: waited 120 s for its answer with none come; 2 batches in \
             flight, 1 pieces queued, 2 records kept in place; its windows: held, held, idle"
        );
        assert!(wait_note("s", 0, 60, (0, 0, 0), None).ends_with("its windows: not a ring task's"));
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
