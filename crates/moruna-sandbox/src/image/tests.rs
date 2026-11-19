//! IM-T26 and IM-T27 (SDD 19 k): the stream read with care onto the plane, and the words of a
//! failure.

use std::io::Cursor;
use std::sync::Arc;

use moruna_kernel::arrow::array::{DictionaryArray, Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Schema};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;

use moruna_plane_linux::stream::Reader;

use super::*;

fn batch(n: i64) -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, false),
            Field::new("s", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from_iter_values(0..n)),
            Arc::new(StringArray::from_iter_values(
                (0..n).map(|i| format!("row {i}")),
            )),
        ],
    )
    .unwrap()
}

/// A stream of `batches` batches, with the end-of-stream marker when `end`.
fn stream(batches: usize, end: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &batch(1).schema()).unwrap();
    for i in 0..batches {
        writer.write(&batch(i as i64 + 2)).unwrap();
    }
    if end {
        writer.finish().unwrap();
    } else {
        drop(writer);
    }
    out
}

/// Every record batch of `bytes`, each laid as a record into a window and checked as the
/// engine checks a put record; or the first refusal.
fn records(bytes: Vec<u8>) -> std::result::Result<Vec<u64>, String> {
    let mut reader = Reader::new(Cursor::new(bytes));
    let mut rows = Vec::new();
    while let Some(pending) = reader.batch()? {
        let mut window = vec![0u8; pending.len() as usize];
        let len = reader.lay(pending, &mut window)?;
        let n = match moruna_ipc_check::record::framing_need(&window)? {
            moruna_ipc_check::record::Need::Framing(n) => n,
            other => return Err(format!("{other:?}")),
        };
        rows.push(moruna_ipc_check::record::check_record(&window[..n], len)?.rows);
    }
    Ok(rows)
}

/// IM-T26 the_output_is_read_with_care: the reader of f.6 lays each record batch of a stream as
/// a record the engine's check passes; the marker and the pipe closed at a boundary end it; a
/// declared length over the bound, a stream that ends inside a message, a dictionary and bytes
/// that are no message are refused with their reason.
#[test]
fn im_t26_the_output_is_read_with_care() {
    assert_eq!(records(stream(2, true)).unwrap(), vec![2, 3]);
    assert_eq!(
        records(stream(1, false)).unwrap(),
        vec![2],
        "closed at a boundary"
    );
    assert_eq!(
        records(Vec::new()).unwrap(),
        Vec::<u64>::new(),
        "nothing at all"
    );
    let whole = stream(1, true);
    for cut in [whole.len() - 12, whole.len() - 200, 6, 2] {
        let e = records(whole[..cut].to_vec()).unwrap_err();
        assert!(
            e.contains("ends inside") || e.contains("ends after"),
            "{cut}: {e}"
        );
    }
    let e = records(b"this is not an Arrow stream\n".to_vec()).unwrap_err();
    assert!(
        e.contains("more than the") || e.contains("not an Arrow"),
        "{e}"
    );
    let e = records(vec![0xff; 4]).unwrap_err();
    assert!(e.contains("continuation"), "{e}");
    let mut hostile = vec![0xff; 4];
    hostile.extend_from_slice(&u32::MAX.to_le_bytes());
    let e = records(hostile).unwrap_err();
    assert!(e.contains("one may hold"), "{e}");
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&8u32.to_le_bytes());
    hostile.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(records(hostile).is_err());
    // A dictionary column: refused as the plane refuses it, before any batch.
    let d: DictionaryArray<Int32Type> = vec!["a", "b", "a"].into_iter().collect();
    let dict = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "d",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            false,
        )])),
        vec![Arc::new(d)],
    )
    .unwrap();
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &dict.schema()).unwrap();
    writer.write(&dict).unwrap();
    writer.finish().unwrap();
    let e = records(out).unwrap_err();
    assert!(e.contains("carries no dictionaries"), "{e}");
    // A window too small for the record is refused, not overrun.
    let mut reader = Reader::new(Cursor::new(stream(1, true)));
    let pending = reader.batch().unwrap().unwrap();
    let mut small = vec![0u8; 4096];
    let e = reader.lay(pending, &mut small).unwrap_err();
    assert!(e.contains("more than the 4096 bytes of the window"), "{e}");
}

/// IM-T27 the_words_of_a_failure.
#[test]
fn im_t27_the_words_of_a_failure() {
    // Image Launch owns and tests user, environment and command-path interpretation.
    // How a process ends.
    let name = "scorer:1";
    assert_eq!(
        describe(Exit::Code(3), false, name),
        "image `scorer:1` exited with status 3"
    );
    let sys = describe(Exit::Signal(libc::SIGSYS), false, name);
    assert!(
        sys.contains("made a system call it is not allowed"),
        "{sys}"
    );
    assert!(describe(Exit::Signal(libc::SIGKILL), true, name).contains("more memory than"));
    assert!(describe(Exit::Signal(libc::SIGKILL), false, name).contains("SIGKILL (9)"));
    assert!(describe(Exit::Signal(libc::SIGSEGV), false, name).contains("SIGSEGV"));
    assert!(describe(Exit::Signal(63), false, name).contains("a signal (63)"));
    assert!(describe(Exit::Lost, false, name).contains("status is not known"));

    // Its standard error, last lines first quoted in order and no more than twenty.
    assert!(with_stderr("head".into(), &[]).ends_with("it wrote nothing to its standard error"));
    let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let quoted = with_stderr("head".into(), &lines);
    assert!(quoted.starts_with("head; the last 20 lines of its standard error:\n  line 10\n"));
    assert!(quoted.ends_with("  line 29"));
    assert_eq!(quoted.lines().count(), 21);
}

/// The real parser/refusal cause survives an observed exit, including successful
/// exits and the SIGKILL used to stop a malformed-output process.
#[test]
fn protocol_refusal_is_retained_alongside_actual_process_exit() {
    let invalid = "its output is not an Arrow IPC stream: invalid continuation marker";
    let head = failure_head("echo", invalid, Some(Exit::Code(0)), false);
    assert!(head.contains("exited with status 0"), "{head}");
    assert!(head.contains(invalid), "{head}");

    let refused = "its output was refused by the engine's check: invalid continuation marker";
    let head = failure_head("echo", refused, Some(Exit::Signal(libc::SIGKILL)), false);
    assert!(head.contains("SIGKILL (9)"), "{head}");
    assert!(head.contains(refused), "{head}");
    assert_eq!(
        failure_head("echo", refused, None, false),
        format!("image `echo`: {refused}")
    );
}

/// An image that was never unpacked: a root that exists, a command, and `env` as its config's
/// environment. Enough to make its kernel, which starts no process until `init`.
fn image(env: &[&str]) -> Image {
    Image {
        digest: moruna_image::Digest::of(b"an image"),
        config: moruna_image::Config {
            entrypoint: vec!["/bin/function".into()],
            env: env.iter().map(|e| e.to_string()).collect(),
            ..Default::default()
        },
        rootfs: std::env::temp_dir(),
        skipped: 0,
    }
}

/// The kernel of a function `score` whose image declares `env`, in a job of `cpus` CPUs.
fn kernel_of(env: &[&str], cpus: usize) -> Result<ImageKernel> {
    ImageKernel::new(
        "score",
        &image(env),
        None,
        &BTreeMap::new(),
        PathBuf::from("/sys/fs/cgroup/none"),
        false,
        cpus,
    )
}

/// A run of 4 CPUs whose sampler says it holds `anon` bytes under a ceiling of `ceiling`.
fn sampler(anon: u64, ceiling: u64) -> Arc<dyn Sampler> {
    sampler_of(anon, ceiling, 4.0)
}

/// As [`sampler`], with a CPU quota of `cpu`; 0 is a sampler that knows none.
fn sampler_of(anon: u64, ceiling: u64, cpu: f64) -> Arc<dyn Sampler> {
    Arc::new(
        moruna_testkit::FakeSampler::new().scripted(vec![moruna_kernel::Sample {
            anon_bytes: anon,
            ceiling_bytes: ceiling,
            cpu_limit: cpu,
            ..moruna_kernel::Sample::default()
        }]),
    )
}

fn ctx() -> InitCtx {
    InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(moruna_testkit::FakeAllocator::new()),
    }
}

fn kind_of(kernel: &ImageKernel) -> usize {
    match kernel.kind() {
        KernelKind::Stateful { max_instances } => max_instances.get(),
        KernelKind::Stateless => 0,
    }
}

/// 6.3f's defect, as the kernel's kind: an image function that declares nothing, in a job of 4
/// CPUs, is a stage of 4 instances. It was one process, whatever the job had.
#[test]
fn an_image_function_runs_one_instance_per_cpu_of_the_job_unless_it_declares_fewer() {
    let nothing = kernel_of(&["PATH=/bin"], 4).unwrap();
    assert_eq!(nothing.instances(), 4);
    assert_eq!(kind_of(&nothing), 4);
    let count = |env: &[&str], cpus: usize| kind_of(&kernel_of(env, cpus).unwrap());
    // A function that keeps state across batches declares how many it can run.
    assert_eq!(count(&["MORUNA_PROCESSES=1"], 4), 1);
    assert_eq!(
        count(&["MORUNA_PROCESSES=8"], 2),
        2,
        "a ceiling, not a count"
    );
    assert_eq!(count(&["MORUNA_PROCESSES=3"], 8), 3);
    assert_eq!(count(&[], 1), 1);
    assert_eq!(count(&[], 0), 1);
    assert_eq!(count(&[], 200), instances::MAX);
    // A ceiling that is not one is refused when the kernel is made, naming the function.
    let refused = kernel_of(&["MORUNA_PROCESSES=0"], 4)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("score") && refused.contains("1 to 64"),
        "{refused}"
    );
}

/// The run's memory bounds the instances when the run is bound, before anything counts them:
/// as many of those the rule asks for as the stage's ceiling funds at the least each, one in
/// the whole of it when it funds none, and a refusal only when nothing is left.
#[test]
fn memory_bounds_an_image_s_instances_when_the_run_is_bound() {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    let k = kernel_of(&[], 4).unwrap();
    let outside = k.outside();
    assert_eq!(outside.in_use().unwrap(), 0, "no instance yet");
    // No stage: nothing to write.
    k.limits_changed();

    k.bind_run(sampler(0, GIB), GIB);
    assert_eq!((k.instances(), kind_of(&k)), (4, 4));
    assert_eq!(
        k.note(),
        "image `score`: 4 instances, busy_peak 0 (the most holding a batch at once); \
         instance 0: 0 batches; instance 1: 0 batches; instance 2: 0 batches; \
         instance 3: 0 batches"
    );
    // The rest of the run holds all but 40 MiB: two of the four at 16 MiB each.
    k.bind_run(sampler(GIB - 40 * MIB, GIB), GIB);
    assert_eq!((k.instances(), kind_of(&k)), (2, 2));
    let note = k.note();
    assert!(
        note.starts_with("image `score`: 2 instances")
            && note.ends_with("; score: 2 of 4 instances: the share funds 2"),
        "{note}"
    );
    // Not one least is left: one instance in the whole of it, as before 6.3f.
    k.bind_run(sampler(GIB - MIB, GIB), GIB);
    assert_eq!(k.instances(), 1);
    // A sampler that knows no ceiling: the one the run was bound with.
    k.bind_run(sampler(0, 0), 64 * MIB);
    assert_eq!(k.instances(), 4);
    k.bind_run(sampler(0, 0), 63 * MIB);
    assert_eq!(k.instances(), 3);
    // Nothing is left: one instance, which is refused when it starts, as before.
    k.bind_run(sampler(GIB, GIB), GIB);
    assert_eq!(k.instances(), 1);
    let refused = k.init(&ctx()).err().expect("refused").to_string();
    assert!(refused.contains("the stage ceiling is 0"), "{refused}");
    assert_eq!(outside.in_use().unwrap(), 0, "nothing was started");

    // A sample that fails at the bind leaves the rule's count, and fails the stage at its
    // start with the sampler's own error.
    let failing = moruna_testkit::FakeSampler::new();
    failing.set_read_failure(true);
    let k = kernel_of(&[], 4).unwrap();
    k.bind_run(Arc::new(failing), GIB);
    assert_eq!(k.instances(), 4);
    assert!(k.init(&ctx()).is_err());
    // A run that never bound the stage.
    let k = kernel_of(&["MORUNA_PROCESSES=2"], 4).unwrap();
    assert_eq!(k.instances(), 2);
    let unbound = k.init(&ctx()).err().expect("refused").to_string();
    assert!(unbound.contains("has no ceiling"), "{unbound}");
}

/// The job's CPUs are the run's when it is bound: its CPU quota, rounded up, which is what the
/// engine starts its workers from, so a stage has an instance for each worker. The CPUs the
/// stage's build found stand until then, and for a run that says no quota.
#[test]
fn the_job_s_cpus_are_the_run_s_cpu_quota_once_it_is_bound() {
    const GIB: u64 = 1 << 30;
    let k = kernel_of(&[], 2).unwrap();
    assert_eq!(k.instances(), 2, "as the build found them");
    for (quota, instances) in [
        (4.0, 4),
        // A fractional quota is rounded up, as the engine's workers are.
        (2.5, 3),
        (0.5, 1),
        (1.0, 1),
        (200.0, instances::MAX),
        // A sampler that knows no quota: the build's.
        (0.0, 2),
        (f64::NAN, 2),
    ] {
        k.bind_run(sampler_of(0, GIB, quota), GIB);
        assert_eq!(
            (k.instances(), kind_of(&k)),
            (instances, instances),
            "{quota}"
        );
    }
    // A declared ceiling holds whatever the run's CPUs are.
    let k = kernel_of(&["MORUNA_PROCESSES=2"], 1).unwrap();
    assert_eq!(k.instances(), 1);
    k.bind_run(sampler_of(0, GIB, 8.0), GIB);
    assert_eq!(k.instances(), 2);
    assert!(
        !k.note().contains(" of "),
        "memory funded both: {}",
        k.note()
    );
    // And memory bounds the run's count, not the build's: 8 CPUs, 40 MiB.
    let k = kernel_of(&[], 1).unwrap();
    k.bind_run(sampler_of(GIB - (40 << 20), GIB, 8.0), GIB);
    assert_eq!(k.instances(), 2);
    assert!(
        k.note()
            .ends_with("; score: 2 of 8 instances: the share funds 2"),
        "{}",
        k.note()
    );
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The least morsel the run plans (the runtime's `MORSEL_MIN_BYTES`).
const LEAST_MORSEL: u64 = 4 * MIB;

/// A function measured to hold `fixed` bytes whatever its batch and `per_byte` a byte of it,
/// with no spread, over units of 1 to 16 MiB.
fn line(fixed: u64, per_byte: f64) -> moruna_cost::Memory {
    moruna_cost::Memory {
        fixed_bytes: fixed as f64,
        per_input_byte: per_byte,
        output_per_input_byte: None,
        spread: 0.0,
        units: 6,
        input_bytes_min: MIB,
        input_bytes_max: 16 * MIB,
    }
}

/// PM, 2026-11-18 (6.3f): an unmeasured function's stage is funded from the run's room past
/// the arena, each instance at the largest batch it may give one times the function's factor,
/// and its batches are bounded by the share over that factor; a job that ran in one process
/// runs in as many as that funds and never fails for the count.
#[test]
fn an_unmeasured_function_is_funded_at_the_largest_batch_times_the_factor() {
    // Score at 4 GiB into a count sink: 2.5 GiB past the arena, batches of up to 512 MiB, and a
    // function that declares no factor (the run's default, 4).
    let k = kernel_of(&[], 4).unwrap();
    assert_eq!(k.morsel_bytes_max(), None, "nothing said yet");
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    assert_eq!(k.morsel_bytes_max(), None, "no share yet");
    k.bind_run(sampler(0, 4 * GIB), 4 * GIB);
    assert_eq!(k.instances(), 1);
    let note = k.note();
    assert!(
        note.contains(
            "; score: 1 of 4 instances: the share funds 1; unmeasured: bounded by the share, \
             batches of at most 1073741824 bytes, an instance's share of 4294967296 bytes over \
             4.00; each instance funded for 2147483648 bytes, the largest batch of 536870912 \
             bytes times 4.00, of 2684354560 bytes past the arena; each told 4 of the job's 4 \
             CPUs"
        ),
        "{note}"
    );
    // One instance in the whole stage ceiling: a quarter of it a batch.
    assert_eq!(k.morsel_bytes_max(), Some(GIB));
    // Batches of at most 32 MiB (a file sink's lane): 128 MiB each, all four, each held to a
    // quarter of its share of the ceiling.
    k.bind_room(2560 * MIB, 32 * MIB, LEAST_MORSEL, 4.0, None);
    k.bind_run(sampler(0, 4 * GIB), 4 * GIB);
    assert_eq!(k.instances(), 4);
    assert_eq!(k.morsel_bytes_max(), Some(256 * MIB));
    assert!(
        k.note().contains("each told 1 of the job's 4 CPUs"),
        "{}",
        k.note()
    );
    // The room is the run's past its arena, not what the stage's ceiling is at the bind: none
    // left funds one instance, which starts with the stage's ceiling, as before.
    k.bind_room(0, 32 * MIB, LEAST_MORSEL, 4.0, None);
    k.bind_run(sampler(0, 4 * GIB), 4 * GIB);
    assert_eq!(k.instances(), 1);
    // A declared factor below the default funds more, and bounds a batch less.
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 1.25, None);
    k.bind_run(sampler(0, 4 * GIB), 4 * GIB);
    assert_eq!(k.instances(), 4);
    assert_eq!(k.morsel_bytes_max(), Some(858_993_459));
    // A factor that is not one: the least need, and the share itself bounds a batch.
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, f64::NAN, None);
    k.bind_run(sampler(0, 4 * GIB), 4 * GIB);
    assert_eq!(k.instances(), 4);
    assert_eq!(k.morsel_bytes_max(), Some(GIB));
}

/// System design 3A (PM, 2026-11-19, 6.3f and 6.3v): a measured function's stage is sized as
/// the node sizes its tasks: at the largest of the run's pieces at which every CPU of the job
/// runs an instance by its line, each instance's cgroup carved at the line's account there and
/// its batches cut at that piece, whatever the share; the piece shrinks first, and only then do
/// fewer instances run.
#[test]
fn a_measured_function_runs_every_cpu_and_its_line_sets_the_piece() {
    // Score's line, about: 150 MiB of interpreter and libraries, and 20 bytes a byte of input.
    let score = line(150 * MIB, 20.0);
    let k = kernel_of(&[], 4).unwrap();
    // 2.5 GiB past the arena funds four at 16 MiB pieces, 150 + 320 MiB each, although a
    // 512 MiB batch at four instances would not fit.
    k.bind_room(
        2560 * MIB,
        512 * MIB,
        LEAST_MORSEL,
        4.0,
        Some(score.clone()),
    );
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!(k.instances(), 4);
    // Batches of the measured piece, 16 MiB, at accounts of 150 + 20 x 16 MiB.
    assert_eq!(k.morsel_bytes_max(), Some(16 * MIB));
    assert_eq!(*lock(&k.measured), Some((16 * MIB, 470 * MIB)));
    let note = k.note();
    assert!(
        note.contains(
            "measured: batches of at most 16777216 bytes, the stage's measured piece; each \
             instance's cgroup carved at its function's line there, an account of 492830720 \
             bytes (157286400 bytes plus 20.00 a byte, margin 0.00), of 2684354560 bytes past \
             the arena"
        ),
        "{note}"
    );
    assert!(!note.contains(" of 4 instances"), "all four: {note}");
    // The ceiling moved: a measured stage's batches stay at its piece, as the node's do.
    k.share.store(320 * MIB, Ordering::Relaxed);
    assert_eq!(k.morsel_bytes_max(), Some(16 * MIB));
    // Room for two at the least morsel: two, not one, at the least piece.
    k.bind_room(500 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, Some(score.clone()));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!(k.instances(), 2);
    assert_eq!(*lock(&k.measured), Some((LEAST_MORSEL, 230 * MIB)));
    assert!(
        k.note()
            .contains("score: 2 of 4 instances: the share funds 2"),
        "{}",
        k.note()
    );
    // A line that holds nothing whatever its batch: the run's largest morsel, nothing smaller.
    let flat = moruna_cost::Memory {
        input_bytes_max: 0,
        ..line(0, 0.0)
    };
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, Some(flat));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!(k.morsel_bytes_max(), Some(512 * MIB));
    // A stage the run says no line of is on the share's rule: what an instance's share holds.
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!(*lock(&k.measured), None);
    // Measured, but not one instance at the least piece by it: the share's rule, by its line.
    k.bind_room(200 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, Some(score));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!((k.instances(), *lock(&k.measured)), (1, None));
    let note = k.note();
    assert!(
        note.contains("measured: batches of at most ")
            && note.contains("what an instance's share of 2684354560 bytes holds"),
        "{note}"
    );
    // A line whose need at the least morsel is past what a number holds funds one instance.
    let past = line(0, f64::MAX);
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, Some(past));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    assert_eq!(k.instances(), 1);
}

/// 6.3f, system design 3A: a batch past an instance's cap is cut into slices of its rows, each
/// at most the cap, every row once and in order; one that fits, one row, or a stage nothing
/// bounds is given whole. The answers are joined in the order the slices were cut.
#[test]
fn a_batch_past_the_cap_goes_in_slices_and_its_answers_join_in_order() {
    let fake = Arc::new(moruna_testkit::FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = Arc::clone(&fake) as Arc<dyn Allocator>;
    let whole = batch(100_000);
    let len = laid_bytes(&whole, &*alloc).unwrap();
    assert!(len > 1_000_000, "{len}");
    // Nothing bounds it, or it fits: whole.
    assert_eq!(pieces(&whole, None, &*alloc).unwrap().len(), 1);
    assert_eq!(pieces(&whole, Some(len), &*alloc).unwrap().len(), 1);
    let one = batch(1);
    assert_eq!(pieces(&one, Some(1), &*alloc).unwrap().len(), 1);
    // A quarter of it: five slices (nine tenths of a quarter), each within the cap.
    let cap = len / 4;
    let slices = pieces(&whole, Some(cap), &*alloc).unwrap();
    assert_eq!(slices.len(), 5);
    let mut next = 0i64;
    for slice in &slices {
        assert!(laid_bytes(slice, &*alloc).unwrap() <= cap);
        let n = slice
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for v in n.values() {
            assert_eq!(*v, next);
            next += 1;
        }
    }
    assert_eq!(next, 100_000, "every row once, in order");
    // Rows of uneven size: a slice still past the cap is cut again.
    let uneven = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, false)])),
        vec![Arc::new(StringArray::from_iter_values((0..1000).map(
            |i| {
                if i < 100 {
                    "x".repeat(10_000)
                } else {
                    "y".into()
                }
            },
        )))],
    )
    .unwrap();
    let cap = laid_bytes(&uneven, &*alloc).unwrap() / 8;
    let slices = pieces(&uneven, Some(cap), &*alloc).unwrap();
    assert!(slices.len() > 8, "{}", slices.len());
    for slice in &slices {
        assert!(
            slice.num_rows() == 1 || laid_bytes(slice, &*alloc).unwrap() <= cap,
            "{} rows",
            slice.num_rows()
        );
    }
    assert_eq!(
        slices.iter().map(RecordBatch::num_rows).sum::<usize>(),
        1000
    );
    // The answers joined, in order, landed in the run's arena.
    let answers: Vec<Payload> = slices
        .iter()
        .map(|s| {
            moruna_adapters::land::land_table(s.clone(), &alloc)
                .unwrap()
                .0
        })
        .collect();
    let Payload::Table(joined, _) = join("score", answers, &alloc).unwrap() else {
        panic!("a table");
    };
    assert_eq!(joined, uneven);
    // An answer that is not a table, and none at all, are refused naming the function.
    let tensor = Payload::tensor(
        moruna_kernel::ManagedTensor::from_buffer(
            fake.buffer(16, moruna_kernel::Tier::Host),
            0,
            moruna_kernel::DType::F32,
            vec![4],
        )
        .unwrap(),
    )
    .unwrap();
    let refused = join("score", vec![tensor], &alloc)
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("`score`: an answer that is not a table"),
        "{refused}"
    );
    let refused = join("score", Vec::new(), &alloc).err().unwrap().to_string();
    assert!(refused.contains("`score`: no answer to join"), "{refused}");
    // Answers whose schemas differ cannot be one batch: refused, naming the slices.
    let mixed = vec![
        moruna_adapters::land::land_table(batch(2), &alloc)
            .unwrap()
            .0,
        moruna_adapters::land::land_table(uneven.slice(0, 1), &alloc)
            .unwrap()
            .0,
    ];
    let refused = join("score", mixed, &alloc).err().unwrap().to_string();
    assert!(refused.contains("the 2 slices of one batch"), "{refused}");
}

/// A stage's cgroups over a plain directory that stands in for a cgroup v2 parent with `memory`
/// and `cpu` delegated, as the stage module's own tests use one: `ceiling` for `count`.
fn plain_stage(name: &str, ceiling: u64, count: usize) -> (PathBuf, Arc<Stage>) {
    let dir = std::env::temp_dir().join(format!("moruna-warm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cgroup.subtree_control"), "cpu memory\n").unwrap();
    let stage = Arc::new(Stage::create(&dir, "stage", ceiling, 4.0, count).unwrap());
    (dir, stage)
}

/// Score's readings, about: 150 MiB of interpreter and libraries and 20 bytes a byte, at the
/// warm-up's pieces of 1 to 16 MiB.
fn score_readings() -> Vec<(u64, u64)> {
    warm::PIECES
        .iter()
        .map(|&piece| (piece, 150 * MIB + 20 * piece))
        .collect()
}

/// System design 3A, on Linux as on the node (PM, 2026-11-19, 6.3f): a function the run has no
/// measurement of warms its stage up on its first instance, which alone is given work; its
/// readings are fitted, and the stage's instances are funded from the fit (the piece shrinks
/// first, then the count), its ceiling shared among them, the room they do not need handed
/// back, and the others start. The note gives the pieces, the time and the fit.
#[test]
fn an_unmeasured_function_warms_up_on_its_first_instance_and_the_fit_funds_the_others() {
    let freed = Arc::new(AtomicU64::new(0));
    let k = kernel_of(&[], 4).unwrap();
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    let told = Arc::clone(&freed);
    k.warm_up(
        None,
        Box::new(move |bytes| {
            told.fetch_add(bytes, Ordering::Relaxed);
        }),
    );
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    // While it warms up: four asked, at the ceiling's least each, only the first given work,
    // and its batches bounded by its share as an unmeasured function's are.
    assert!(k.warming());
    assert_eq!((k.instances(), k.instances_ready()), (4, 1));
    assert_eq!(k.morsel_bytes_max(), Some(160 * MIB));
    let (dir, stage) = plain_stage("fit", 2560 * MIB, 4);
    lock(&k.warm).readings = score_readings();
    lock(&k.warm).started = Some(Instant::now());
    k.warmed(&stage);
    assert!(!k.warming());
    // The fit: 150 MiB of its own and 20 bytes a byte (to the least squares' rounding). Four at
    // pieces of 16 MiB need 470 MiB each, of the 2560 the run has for them: all four, and
    // 680 MiB back to the arena.
    let near = |got: u64, want: u64| got.abs_diff(want) <= 64;
    assert_eq!((k.instances(), k.instances_ready()), (4, 4));
    let back = freed.load(Ordering::Relaxed);
    assert!(near(back, 680 * MIB), "{back}");
    assert_eq!(stage.share(), 640 * MIB);
    // Batches of the piece the fit sized the instances at (6.3v), as the node's are cut.
    let cap = k.morsel_bytes_max().expect("a cap");
    assert_eq!(cap, 16 * MIB);
    let note = k.note();
    assert!(
        note.contains("; warmed up on its first instance over 5 pieces in ")
            && note.contains(
                " ms (1048576 bytes to a peak of 178257920, 2097152 bytes to a peak of 199229440, "
            ),
        "{note}"
    );
    assert!(
        note.contains(
            " bytes of its own and 20.000 bytes a byte, margin 0.00; its 4 instances \
             funded at pieces of 16777216 bytes, "
        ) && note.contains(&format!(
            " bytes each, of the 2684354560 bytes the run has for them, {back} bytes of it \
                 handed back to the arena"
        )) && note.contains(&format!("; measured: batches of at most {cap} bytes")),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A room that funds two at the least piece: two, the piece first shrunk to the least.
    let k = kernel_of(&[], 4).unwrap();
    k.bind_room(500 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    k.warm_up(None, Box::new(|_| {}));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    let (dir, stage) = plain_stage("two", 2560 * MIB, 4);
    lock(&k.warm).readings = score_readings();
    k.warmed(&stage);
    assert_eq!((k.instances(), k.instances_ready()), (2, 2));
    assert_eq!(stage.share(), 1280 * MIB);
    let note = k.note();
    assert!(
        note.contains("its 2 instances funded at pieces of 4194304 bytes")
            && note.contains("score: 2 of 4 instances: the share funds 2"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A room that funds not one at the least: one, in the whole of the stage's ceiling.
    let k = kernel_of(&[], 4).unwrap();
    k.bind_room(100 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    k.warm_up(None, Box::new(|_| {}));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    let (dir, stage) = plain_stage("one", 2560 * MIB, 4);
    lock(&k.warm).readings = score_readings();
    k.warmed(&stage);
    assert_eq!((k.instances(), stage.share()), (1, 2560 * MIB));
    assert!(
        k.note()
            .contains("; one instance, in the whole of the stage's ceiling ("),
        "{}",
        k.note()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The warm-up keeps the slope of what the run had of the function (a check's few units), and
/// takes its fixed term from its own readings; a stage of one instance has nothing to size and
/// does not warm up; a run that said no room leaves the stage as it was; no reading, no fit.
#[test]
fn a_warm_up_keeps_the_slope_it_came_with_and_needs_others_to_size() {
    let k = kernel_of(&[], 4).unwrap();
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    // A check's line: a slope of 18 bytes a byte, and a fixed term with no interpreter in it.
    k.warm_up(Some(line(MIB, 18.0)), Box::new(|_| {}));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    let (dir, stage) = plain_stage("slope", 2560 * MIB, 4);
    lock(&k.warm).readings = score_readings();
    k.warmed(&stage);
    let note = k.note();
    // The fixed term is the most of peak - 18 x piece: at 16 MiB, 150 + 2 x 16 MiB.
    assert!(
        note.contains(": 190840832 bytes of its own and 18.000 bytes a byte")
            && note.contains("(the slope of the measurement it came with)"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // One instance: nothing to size, so no warm-up.
    let one = kernel_of(&["MORUNA_PROCESSES=1"], 4).unwrap();
    one.warm_up(None, Box::new(|_| {}));
    one.bind_run(sampler(0, GIB), GIB);
    assert!(!one.warming());
    assert_eq!(one.instances_ready(), 1);

    // No room said: the fit is said, the stage stays as it was.
    let k = kernel_of(&[], 4).unwrap();
    k.warm_up(None, Box::new(|_| {}));
    k.bind_run(sampler(0, GIB), GIB);
    let (dir, stage) = plain_stage("none", GIB, 4);
    lock(&k.warm).readings = score_readings();
    k.warmed(&stage);
    assert_eq!((k.instances(), stage.share()), (4, GIB / 4));
    assert!(
        k.note()
            .contains("; the run said no room for the stage, which stays as it was"),
        "{}",
        k.note()
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A job of tiny batches of one size (PM, 2026-11-19): its fit has no slope and is scaled
    // in proportion past them, so costed at the least morsel an instance needs ~1.3 GiB; costed
    // down to what was measured, 3 MiB: every instance runs.
    let k = kernel_of(&[], 4).unwrap();
    k.bind_room(2560 * MIB, 512 * MIB, LEAST_MORSEL, 4.0, None);
    k.warm_up(None, Box::new(|_| {}));
    k.bind_run(sampler(0, 2560 * MIB), 4 * GIB);
    let (dir, stage) = plain_stage("tiny", 2560 * MIB, 4);
    lock(&k.warm).readings = vec![(9 << 10, 3 * MIB); 5];
    k.warmed(&stage);
    assert_eq!((k.instances(), k.instances_ready()), (4, 4));
    assert!(
        k.note()
            .contains("; its 4 instances funded at pieces of 9216 bytes, 3145728 bytes each"),
        "{}",
        k.note()
    );
    let _ = std::fs::remove_dir_all(&dir);

    // No reading at all: nothing fitted, the warm-up ends and says what it had.
    let k = kernel_of(&[], 4).unwrap();
    k.warm_up(None, Box::new(|_| {}));
    k.bind_run(sampler(0, GIB), GIB);
    let (dir, stage) = plain_stage("empty", GIB, 4);
    k.warmed(&stage);
    assert!(!k.warming());
    assert!(
        k.note()
            .contains("; warmed up on its first instance over 0 pieces in 0 ms ()"),
        "{}",
        k.note()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PM, 2026-11-18 (6.3f): every instance is told its share of the job's CPUs, and the common
/// thread pools are set to it unless the image's environment sets them.
#[test]
fn each_instance_is_told_its_share_of_the_job_s_cpus() {
    const GIB: u64 = 1 << 30;
    let k = kernel_of(&["PATH=/bin", "OMP_NUM_THREADS=3"], 1).unwrap();
    k.bind_run(sampler_of(0, GIB, 4.0), GIB);
    assert_eq!(k.instances(), 4);
    let env = k.instance_env(Some("MORUNA_PORT_SOCKET=fd:3"));
    for given in [
        "MORUNA_CPUS=1",
        "OPENBLAS_NUM_THREADS=1",
        "MKL_NUM_THREADS=1",
        "NUMEXPR_NUM_THREADS=1",
        "RAYON_NUM_THREADS=1",
        "OMP_NUM_THREADS=3",
    ] {
        assert_eq!(
            env.iter().filter(|e| *e == given).count(),
            1,
            "{given}: {env:?}"
        );
    }
    assert!(!env.iter().any(|e| e == "OMP_NUM_THREADS=1"), "{env:?}");
    assert_eq!(
        env.last().map(String::as_str),
        Some("MORUNA_PORT_SOCKET=fd:3")
    );
    // Memory funds two of the four: each is told two.
    k.bind_run(sampler_of(GIB - (40 << 20), GIB, 4.0), GIB);
    assert_eq!(k.instances(), 2);
    assert!(k.instance_env(None).contains(&"MORUNA_CPUS=2".to_string()));
    // A function that declares one instance has the job's CPUs to itself.
    let one = kernel_of(&["MORUNA_PROCESSES=1"], 1).unwrap();
    one.bind_run(sampler_of(0, GIB, 8.0), GIB);
    assert!(
        one.instance_env(None)
            .contains(&"OPENBLAS_NUM_THREADS=8".to_string())
    );
}
