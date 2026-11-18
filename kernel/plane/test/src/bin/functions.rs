//! Item 5.2b (system design 5B): C functions on the core. An engine on `RingPlane` runs three
//! native functions that `kernel/function/c/build-core.sh` builds, each one static program of C
//! on relibc and std with the C SDK (`moruna_function.h`), over the plane, with batches of one
//! `Int64` column of 128 Ki known values (the `mib` fixture's framing), and judges every row:
//!
//! - `c-double` (examples/double.c): each row of its records is twice its input's;
//! - `c-runtime` (tests/c/runtime.c): each batch copied on a thread of its own (pthread_create,
//!   malloc, a thread_local) and written back, each row its input's, with a line of standard
//!   error (fprintf) for each batch in its log;
//! - `c-sum` (the aggregate test): a record of -1 before the first batch is taken, then the
//!   column's total, which comes after the engine ends its input;
//! - `c-params` (tests/c/params.c, item 5.5): given `{"threshold":0.5}` as `MORUNA_PARAMS` in
//!   its start slot `env`, as the engine's stage gives a native function its environment, it
//!   writes 0.5 back as its one record, read through `moruna_params()` and `getenv` alike, and
//!   says its four variables in its log.
//!
//! Each task is given its environment as the engine gives one (`moruna_sys::env`), says exit
//! code 0 on its rings and ends.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use moruna_ipc_check::{frame, record};
use moruna_plane::{
    BatchId, DataPlane, Event, PlaneError, RingPlane, TaskId,
    kernel::Kernel,
    ringed::{Region, RingShape, View},
};
use moruna_record::{record::Decoded, ring::Ring};
use moruna_svc::{Channel, Console, Handle, deadline_in, println, start};
use moruna_sys::{Account, Error, Memory, Process, Rights, map, plane::attach};

moruna_svc::main!(run);

const PAGE: u64 = 4096;
const MIB: u64 = 1 << 20;
const SECONDS: u64 = 1_000_000_000;
/// The engine's plane: a task's region and its windows.
const BUDGET: u64 = 32 * MIB;
/// A task's account: its process, its image, its threads' stacks and its heap.
const TASK_LIMIT: u64 = 48 * MIB;
/// Batches each function is given.
const BATCHES: u64 = 4;
/// Values of a batch, as the `mib` fixture's framing declares.
const ROWS: u64 = 128 << 10;
/// Values the engine writes at a time.
const CHUNK: usize = 8192;
/// The task's log ring, as the engine's stage gives one.
const LOG_BYTES: u64 = 64 << 10;
/// What `c-runtime` says on standard error for each batch.
const COPIED: &str = "copied 131072 rows on a thread of its own";
/// What `c-params` is given: the four variables every function's process has (item 5.5), as
/// the engine's stage gives a native function them, its entry its name.
const PARAMS_ENV: [&str; 4] = [
    "MORUNA_ARGS={}",
    "MORUNA_ENTRY=c-params",
    "MORUNA_PARAMS={\"threshold\":0.5}",
    "MORUNA_ROLE=function",
];
/// The line `c-params` says of them in its log.
const PARAMS_SAID: &str =
    "params: entry c-params, role function, params {\"threshold\":0.5}, args {}";

type Plane = RingPlane<Kernel>;

fn plane_error(e: PlaneError) -> String {
    format!("{e:?}")
}

fn kernel_error(what: &str) -> impl FnOnce(Error) -> String + '_ {
    move |e| format!("{what}: {}", e.name())
}

/// What a data handle of the boot holds: a `u64` length and then that many bytes.
fn fixture(name: &str) -> Result<Vec<u8>, String> {
    let handle = start::take(name).ok_or_else(|| format!("no fixture `{name}`"))?;
    let memory = Memory(handle);
    let head = memory
        .map(0, PAGE, map::READ)
        .map_err(kernel_error("mapping a fixture"))?;
    let mut word = [0u8; 8];
    head.read(0, &mut word)
        .map_err(kernel_error("reading a fixture"))?;
    drop(head);
    let len = u64::from_le_bytes(word);
    let span = (8 + len).div_ceil(PAGE) * PAGE;
    let whole = memory
        .map(0, span, map::READ)
        .map_err(kernel_error("mapping a fixture"))?;
    let mut bytes = vec![0u8; len as usize];
    whole
        .read(8, &mut bytes)
        .map_err(kernel_error("reading a fixture"))?;
    Ok(bytes)
}

fn take_resource(out: &Console) -> Option<Handle> {
    let supervisor = Channel(start::take("supervisor")?);
    let mut buf = [0u8; 16];
    let mut handles = [None];
    if let Err(e) = moruna_svc::wait_one(
        &supervisor.0,
        moruna_svc::Signals::READABLE | moruna_svc::Signals::PEER_CLOSED,
        deadline_in(60 * SECONDS),
    ) {
        println!(out, "FN functions: FAIL: no process resource: {}", e.name());
        return None;
    }
    supervisor.read(&mut buf, &mut handles).ok()?;
    handles[0].take()
}

/// The value `x` of row `j` of batch `n`.
fn x_of(n: u64, j: u64) -> i64 {
    (n * 1_000_000 + j) as i64
}

/// The input batches' layout, as their framing declares it: the record's length, where its
/// values start, and its validity bitmap, if the column has one.
struct Layout {
    framing: Vec<u8>,
    values: u64,
    validity: Option<(u64, u64)>,
    len: u64,
}

impl Layout {
    fn of(mut framing: Vec<u8>) -> Result<Layout, String> {
        // The fixture holds the framing padded to a page; the record check takes its two
        // messages alone.
        match record::framing_need(&framing)? {
            record::Need::Framing(end) if end <= framing.len() => framing.truncate(end),
            _ => return Err(String::from("the fixture holds no whole framing")),
        }
        let batch = 8 + frame::message_len(&framing)?;
        let body = frame::body_length(framing.get(batch..).ok_or("no batch message")?)?;
        let len = framing.len() as u64 + u64::try_from(body).map_err(|_| "a negative body")?;
        let record = record::check_record(&framing, len)?;
        if record.rows != ROWS {
            return Err(format!("the framing declares {} rows", record.rows));
        }
        let values = record
            .buffers
            .iter()
            .find(|(_, l)| *l == ROWS * 8)
            .map(|(at, _)| *at)
            .ok_or("no buffer of the values")?;
        let validity = record
            .buffers
            .iter()
            .copied()
            .find(|(_, l)| *l > 0 && *l != ROWS * 8);
        Ok(Layout {
            framing,
            values,
            validity,
            len,
        })
    }
}

/// A function's task while it runs.
struct Task {
    id: TaskId,
    name: &'static str,
    /// Its process and account, kept while it lives.
    held: Option<(Process, Account)>,
    /// The log ring it was given, which the engine reads.
    log: Memory,
    ended: bool,
    /// Its records not yet judged, as they came.
    puts: Vec<BatchId>,
    /// Its releases of the batches it was given.
    released: u64,
}

struct Run {
    out: Console,
    plane: Plane,
    resource: Handle,
    layout: Layout,
}

impl Run {
    /// Starts `image` as task `name` on a ring of its own, with its log ring and `env` as its
    /// environment.
    fn start(&mut self, name: &'static str, image: &Handle, env: &[&str]) -> Result<Task, String> {
        let account = Account::carve(TASK_LIMIT, name).map_err(kernel_error("account_create"))?;
        let process = Process::create_in(&self.resource, name, &account.0)
            .map_err(kernel_error("process_create"))?;
        let entry = process
            .map_image(image)
            .map_err(kernel_error("process_map_image"))?;
        // A window holds a record twice the input's: the function's records are its own.
        let (id, port) = self
            .plane
            .open_ring(RingShape {
                region: Region::New(4 * MIB),
                windows: 4,
                window_bytes: (2 * self.layout.len).div_ceil(PAGE) * PAGE,
            })
            .map_err(plane_error)?;
        let slots = self.plane.ring_slots(id).map_err(plane_error)?;
        attach(&process, &slots, Some(port))
            .map_err(|(e, _)| format!("process_plane: {}", e.name()))?;
        let log = log_ring()?;
        let given = log
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(kernel_error("handle_duplicate"))?;
        let env = moruna_sys::env::give(env).map_err(kernel_error("its environment"))?;
        process
            .start_with(
                entry,
                &mut [(Some(given), "log"), (Some(env), moruna_sys::env::SLOT)],
            )
            .map_err(kernel_error("process_start"))?;
        Ok(Task {
            id,
            name,
            held: Some((process, account)),
            log,
            ended: false,
            puts: Vec::new(),
            released: 0,
        })
    }

    /// Makes batch `n` in the task's region and gives it to the task.
    fn post(&mut self, t: &Task, n: u64) -> Result<(), String> {
        let region = self.plane.region_of(t.id).ok_or("no region")?;
        let layout = &self.layout;
        let mut chunk = vec![0u8; CHUNK * 8];
        let (b, rows) = self
            .plane
            .load_into(region, layout.len, |v, at| {
                v.write(at, &layout.framing)?;
                // Every value is valid.
                if let Some((bitmap, bytes)) = layout.validity {
                    let ones = vec![0xffu8; bytes as usize];
                    v.write(at + bitmap, &ones)?;
                }
                let mut j = 0;
                while j < ROWS {
                    let k = (ROWS - j).min(CHUNK as u64);
                    let (words, _) = chunk.as_chunks_mut::<8>();
                    for (i, w) in words.iter_mut().take(k as usize).enumerate() {
                        *w = x_of(n, j + i as u64).to_le_bytes();
                    }
                    v.write(at + layout.values + j * 8, &chunk[..k as usize * 8])?;
                    j += k;
                }
                Ok(())
            })
            .map_err(plane_error)?;
        if rows != ROWS {
            return Err(format!("batch {n} holds {rows} rows"));
        }
        self.plane.offer(t.id, 0, b).map_err(plane_error)?;
        self.plane.drop_batch(b).map_err(plane_error)
    }

    /// Takes the plane's events for a quarter of a second at most: a task that releases a batch
    /// and posts nothing rings no bell, and its release is read at the deadline.
    fn look(&mut self, t: &mut Task) -> Result<(), String> {
        let Some(e) = self
            .plane
            .next(deadline_in(SECONDS / 4))
            .map_err(plane_error)?
        else {
            return Ok(());
        };
        match e {
            Event::Put { batch, .. } => t.puts.push(batch),
            Event::Released { .. } => t.released += 1,
            Event::Ended { .. } => t.ended = true,
            Event::Refused { reason, .. } => return Err(format!("a record refused: {reason}")),
            Event::Call { call, .. } => self
                .plane
                .answer(call, Err(Error::AccessDenied))
                .map_err(plane_error)?,
            Event::Freed { .. } => {}
        }
        Ok(())
    }

    /// Looks until `done` holds of the task, for 30 seconds at most.
    fn until(
        &mut self,
        t: &mut Task,
        what: &str,
        done: impl Fn(&Task) -> bool,
    ) -> Result<(), String> {
        let deadline = deadline_in(30 * SECONDS);
        while !done(t) {
            if moruna_svc::now() >= deadline {
                return Err(format!("{what} within 30 s"));
            }
            if t.ended && !done(t) {
                return Err(format!("the task ended before {what}"));
            }
            self.look(t)?;
        }
        Ok(())
    }

    /// The values of a record of one `Int64` column, its last buffer, row by row.
    fn values(&self, batch: BatchId) -> Result<Vec<i64>, String> {
        let (view, at, len) = self
            .plane
            .batch_view(batch)
            .ok_or("the record is not a ring batch")?;
        let mut bytes = vec![0u8; len as usize];
        view.read(at, &mut bytes)
            .map_err(kernel_error("reading a record"))?;
        let end = match record::framing_need(&bytes)? {
            record::Need::Framing(end) if end <= bytes.len() => end,
            _ => return Err(String::from("a record without a whole framing")),
        };
        let checked = record::check_record(&bytes[..end], len)?;
        let rows = checked.rows;
        let &(start, size) = checked.buffers.last().ok_or("a record with no buffer")?;
        if size < rows * 8 {
            return Err(format!("a values buffer of {size} bytes for {rows} rows"));
        }
        let words = bytes
            .get(start as usize..(start + rows * 8) as usize)
            .ok_or("values past the record's end")?;
        let (words, _) = words.as_chunks::<8>();
        Ok(words.iter().map(|w| i64::from_le_bytes(*w)).collect())
    }

    /// Ends the task's input, takes what it posts after, and waits for its end: it said exit
    /// code 0 on its rings.
    fn finish(&mut self, t: &mut Task) -> Result<(), String> {
        self.plane.finish(t.id, 0).map_err(plane_error)?;
        self.until(t, "its end", |t| t.ended)?;
        t.held = None;
        match self.plane.exit_code(t.id) {
            Some(0) => Ok(()),
            said => Err(format!("it said exit code {said:?} on its rings, not 0")),
        }
    }

    /// A function of rows: `BATCHES` batches one at a time, each answered by records whose rows
    /// are `f` of the batch's, in order.
    fn rows(&mut self, t: &mut Task, f: fn(i64) -> i64) -> Result<(), String> {
        for n in 0..BATCHES {
            self.post(t, n)?;
            let mut row = 0;
            while row < ROWS {
                self.until(t, "a record", |t| !t.puts.is_empty())?;
                let record = t.puts.remove(0);
                let values = self.values(record)?;
                for (i, v) in values.iter().enumerate() {
                    let j = row + i as u64;
                    if j >= ROWS || *v != f(x_of(n, j)) {
                        return Err(format!("batch {n} row {j} is {v}"));
                    }
                }
                row += values.len() as u64;
                self.plane.drop_batch(record).map_err(plane_error)?;
            }
        }
        self.finish(t)?;
        match t.puts.len() {
            0 => Ok(()),
            n => Err(format!("{n} records after its input's end")),
        }
    }

    /// A failed function: its log, and its process ended.
    fn fail(&mut self, t: &mut Task, why: &str) {
        println!(self.out, "FN {}: FAIL: {}", t.name, why);
        if let Ok(lines) = log_lines(&t.log) {
            for (stream, _, text) in lines.iter().take(20) {
                println!(self.out, "FN {}: log {}: {}", t.name, stream, text);
            }
        }
        if let Some((process, _)) = t.held.take() {
            let _ = process.kill();
        }
    }
}

/// A formatted log ring, as the engine's stage makes one for a task.
fn log_ring() -> Result<Memory, String> {
    let memory = Memory::create(LOG_BYTES).map_err(kernel_error("memory_create"))?;
    {
        let mut mapping = memory
            .map(0, LOG_BYTES, map::READ | map::WRITE)
            .map_err(kernel_error("memory_map"))?;
        let region = mapping
            .shared_region()
            .map_err(kernel_error("the log ring's region"))?;
        Ring::init(region).map_err(|e| format!("formatting the log ring: {e:?}"))?;
    }
    Ok(memory)
}

/// The lines in a task's log ring: each record's `stream` header, its subject and its text.
fn log_lines(memory: &Memory) -> Result<Vec<(String, u32, String)>, String> {
    let mut mapping = memory
        .map(0, LOG_BYTES, map::READ | map::WRITE)
        .map_err(kernel_error("memory_map"))?;
    let region = mapping
        .shared_region()
        .map_err(kernel_error("the log ring's region"))?;
    let ring = Ring::attach(region).map_err(|e| format!("attaching the log ring: {e:?}"))?;
    let mut consumer = ring
        .consumer()
        .map_err(|e| format!("the log ring's consumer: {e:?}"))?;
    let mut out = Vec::new();
    while let Some(bytes) = consumer.pop() {
        match moruna_record::record::decode(bytes) {
            Decoded::Record(r, _) => {
                let stream = r
                    .headers
                    .iter()
                    .find(|(k, _)| *k == "stream")
                    .map(|(_, v)| String::from(*v))
                    .unwrap_or_default();
                out.push((
                    stream,
                    r.subject,
                    String::from_utf8_lossy(r.body).into_owned(),
                ));
            }
            other => return Err(format!("a log record that does not decode: {other:?}")),
        }
    }
    Ok(out)
}

fn double(r: &mut Run, t: &mut Task) -> Result<(), String> {
    r.rows(t, |x| x * 2)?;
    println!(
        r.out,
        "FN c-double: ok: {} batches of {} rows given in its region, every row of its records twice its input's, exit code 0 on its rings",
        BATCHES,
        ROWS
    );
    Ok(())
}

fn runtime(r: &mut Run, t: &mut Task) -> Result<(), String> {
    r.rows(t, |x| x)?;
    let lines = log_lines(&t.log)?;
    let copied = lines
        .iter()
        .filter(|(stream, _, text)| stream == "stderr" && text == COPIED)
        .count() as u64;
    if copied != BATCHES {
        return Err(format!(
            "{copied} lines {COPIED:?} on stderr in its log, not {BATCHES}"
        ));
    }
    println!(
        r.out,
        "FN c-runtime: ok: {} batches each copied on a thread of its own (pthread_create, malloc, a thread_local) and every row written back, {} lines of fprintf(stderr) in its log, exit code 0 on its rings",
        BATCHES,
        copied
    );
    Ok(())
}

fn sum(r: &mut Run, t: &mut Task) -> Result<(), String> {
    for n in 0..BATCHES {
        r.post(t, n)?;
        r.until(t, "the batch's release", |t| t.released > n)?;
    }
    let before = t.puts.len();
    r.finish(t)?;
    let total: i64 = (0..BATCHES)
        .flat_map(|n| (0..ROWS).map(move |j| x_of(n, j)))
        .sum();
    let records = core::mem::take(&mut t.puts);
    let mut got = Vec::new();
    for record in &records {
        got.push(r.values(*record)?);
        r.plane.drop_batch(*record).map_err(plane_error)?;
    }
    match got.as_slice() {
        [first, last] if first.as_slice() == [-1] && last.as_slice() == [total] && before <= 1 => {}
        other => {
            return Err(format!(
                "its records held {other:?} ({before} before its input's end), not [-1] and then [{total}] after it"
            ));
        }
    }
    println!(
        r.out,
        "FN c-sum: ok: a record of -1 before its first batch, {} batches released, and the total {} after its input's end, exit code 0 on its rings",
        BATCHES,
        total
    );
    Ok(())
}

fn params(r: &mut Run, t: &mut Task) -> Result<(), String> {
    r.post(t, 0)?;
    r.until(t, "the batch's release", |t| t.released > 0)?;
    r.finish(t)?;
    let records = core::mem::take(&mut t.puts);
    let mut got = Vec::new();
    for record in &records {
        let values = r.values(*record)?;
        got.push(
            values
                .iter()
                .map(|v| f64::from_bits(*v as u64))
                .collect::<Vec<f64>>(),
        );
        r.plane.drop_batch(*record).map_err(plane_error)?;
    }
    if got != [vec![0.5]] {
        return Err(format!(
            "its records held {got:?}, not one of the one value 0.5"
        ));
    }
    let lines = log_lines(&t.log)?;
    if !lines.iter().any(|(_, _, text)| text == PARAMS_SAID) {
        return Err(format!("its log has no line {PARAMS_SAID:?}: {lines:?}"));
    }
    println!(
        r.out,
        "FN c-params: ok: given {} in its start slot env, it read the threshold through moruna_params and getenv alike and wrote 0.5 back as its record, its four variables in its log, exit code 0 on its rings",
        PARAMS_ENV[2]
    );
    Ok(())
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(resource) = take_resource(&out) else {
        return 1;
    };
    let framing = match fixture("mib") {
        Ok(g) => g,
        Err(why) => {
            println!(out, "FN functions: FAIL: {}", why);
            return 1;
        }
    };
    let layout = match Layout::of(framing) {
        Ok(l) => l,
        Err(why) => {
            println!(out, "FN functions: FAIL: the batch's framing: {}", why);
            return 1;
        }
    };
    let mut r = Run {
        out,
        plane: RingPlane::new(Kernel, BUDGET),
        resource,
        layout,
    };
    type Test = fn(&mut Run, &mut Task) -> Result<(), String>;
    let tests: [(&'static str, &str, &[&str], Test); 4] = [
        ("c-double", "double-image", &[], double),
        ("c-runtime", "runtime-image", &[], runtime),
        ("c-sum", "sum-image", &[], sum),
        ("c-params", "params-image", &PARAMS_ENV, params),
    ];
    for (name, image, env, test) in tests {
        let Some(image) = start::take(image) else {
            println!(r.out, "FN {}: FAIL: no image", name);
            return 90;
        };
        let mut t = match r.start(name, &image, env) {
            Ok(t) => t,
            Err(why) => {
                println!(r.out, "FN {}: FAIL: starting it: {}", name, why);
                return 1;
            }
        };
        if let Err(why) = test(&mut r, &mut t) {
            r.fail(&mut t, &why);
            return 1;
        }
    }
    println!(
        r.out,
        "FN functions: ok: four C functions on relibc and std, each one static program"
    );
    0
}
