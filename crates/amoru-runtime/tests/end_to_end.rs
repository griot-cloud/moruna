//! The run lifecycle over real components: a real Parquet file this test writes, a real
//! `ParquetSource`, a real kernel, a real `ParquetSink`, the real arena, reactor, placement
//! engine, scheduler and controller, inside a real budget, producing a real run report.
//!
//! Nothing here is a fake. It is the first thing in the project that runs a pass over a
//! dataset, and it is what PY-I1 and 12 f.1 are for.

#![allow(clippy::result_large_err)]

mod support;

use std::sync::Arc;

use amoru_kernel::{CancelToken, Kernel};
use amoru_runtime::{RunSpec, Runtime, SinkSpec, SourceSpec};
use support::{Doubler, FailOnce, Scratch, one_run_at_a_time, read_back_rows, write_parquet};

/// The end to end pass: rows in equals rows out, and the report says so.
#[test]
fn rt_t1_end_to_end_parquet_kernel_parquet() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t1");
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    let rows: u64 = 40_000;
    write_parquet(&input, rows, 4);

    let kernel: Arc<dyn Kernel> = Arc::new(Doubler::new());
    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());

    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            amoru_sources::ParquetSource::new(
                amoru_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn amoru_kernel::Source>)
        })),
        vec![kernel],
        SinkSpec::Build(Box::new(move |ctx| {
            amoru_sinks::ParquetSink::new(
                amoru_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    // The default `sink.file_bytes` is 1 GiB, which the sink reserves from the
                    // arena in one buffer; a 512 MiB budget cannot hold it (see the report).
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn amoru_kernel::Sink>)
        })),
    );
    spec.budget = Some(1 << 30);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    // preamble 6.7: a test writes only under its own scratch directory. Left unset, the facade
    // resolves the preamble's `~/.amoru/profiles` and the run leaves a profile in the
    // developer's home that the next run then reads.
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");

    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!(
            "the run did not complete: {error}; notes: {:?}",
            error.shutdown_notes
        ),
    };

    assert_eq!(
        report.exit,
        amoru_runtime::ExitReason::Completed,
        "the run did not complete; notes: {:?}",
        report.notes
    );
    let stage_rows: u64 = report
        .stages
        .iter()
        .find(|s| s.stage == 1)
        .map(|s| s.rows_in)
        .unwrap_or(0);
    assert_eq!(stage_rows, rows, "the kernel stage did not see every row");

    let written = read_back_rows(&out_dir);
    assert_eq!(written, rows, "the sink did not write every row");
    assert!(!report.run_id.is_empty(), "the report names the run");
}

/// The zero-kernel pipeline of 12 h: source to sink, no stage rows in the report.
#[test]
fn rt_t2_end_to_end_no_kernels() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t2");
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    let rows: u64 = 8_000;
    write_parquet(&input, rows, 2);

    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            amoru_sources::ParquetSource::new(
                amoru_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn amoru_kernel::Source>)
        })),
        Vec::new(),
        SinkSpec::Build(Box::new(move |ctx| {
            amoru_sinks::ParquetSink::new(
                amoru_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    // The default `sink.file_bytes` is 1 GiB, which the sink reserves from the
                    // arena in one buffer; a 512 MiB budget cannot hold it (see the report).
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink) as Box<dyn amoru_kernel::Sink>)
        })),
    );
    spec.budget = Some(1 << 30);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    // preamble 6.7: a test writes only under its own scratch directory. Left unset, the facade
    // resolves the preamble's `~/.amoru/profiles` and the run leaves a profile in the
    // developer's home that the next run then reads.
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");

    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!("the copy run did not complete: {error}"),
    };
    assert_eq!(report.exit, amoru_runtime::ExitReason::Completed);
    assert_eq!(read_back_rows(&out_dir), rows);
}

/// f.7: a run that terminated left a manifest, and a second run resumes from it, keeps the
/// run's identity and finishes. PY-I9, S17.
#[test]
fn rt_t6_resume_after_a_termination() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t6");
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    let staging = scratch.path().join("staging");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    std::fs::create_dir_all(&staging).expect("the staging directory");
    let rows: u64 = 1_000_000;
    write_parquet(&input, rows, 8);

    let first_kernel: Arc<dyn Kernel> = Arc::new(FailOnce::new(2));
    let error = Runtime::run(
        resume_spec(&input, &out_dir, &staging, true, None, vec![first_kernel]),
        CancelToken::new(),
    )
    .expect_err("the failing kernel terminates the run");
    let manifest = match &error.manifest {
        Some(manifest) => manifest.clone(),
        None => panic!("a terminated run wrote no manifest: {error}"),
    };
    assert!(manifest.exists(), "the manifest the run named is on disk");
    let first_id = error
        .report
        .as_ref()
        .map(|r| r.run_id.clone())
        .expect("the partial report is attached");

    let second_kernel: Arc<dyn Kernel> = Arc::new(FailOnce::new(u64::MAX));
    let second = Runtime::run(
        resume_spec(
            &input,
            &out_dir,
            &staging,
            false,
            Some(manifest),
            vec![second_kernel],
        ),
        CancelToken::new(),
    )
    .expect("the resumed run completes");
    assert!(second.resumed, "the report says the run was resumed");
    assert_eq!(second.run_id, first_id, "a resume keeps the run's identity");

    // The state the run leaves is part of what it asserts (PM, 2026-09-22): the whole test
    // runs again in the same process, over the same input and the same profile store, and
    // must behave identically. It did not: the first pass wrote a profile, and a resumed run
    // that *found* one failed the same way a resumed run that found none did.
    let again = Scratch::new("rt_t6-again");
    let out_dir = again.path().join("out");
    let staging = again.path().join("staging");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    std::fs::create_dir_all(&staging).expect("the staging directory");
    let kernel: Arc<dyn Kernel> = Arc::new(FailOnce::new(2));
    let error = Runtime::run(
        resume_spec(&input, &out_dir, &staging, true, None, vec![kernel]),
        CancelToken::new(),
    )
    .expect_err("the second pass terminates the same way");
    let manifest = match &error.manifest {
        Some(manifest) => manifest.clone(),
        None => panic!("the second pass wrote no manifest: {error}"),
    };
    let kernel: Arc<dyn Kernel> = Arc::new(FailOnce::new(u64::MAX));
    let third = Runtime::run(
        resume_spec(
            &input,
            &out_dir,
            &staging,
            false,
            Some(manifest),
            vec![kernel],
        ),
        CancelToken::new(),
    )
    .expect("the second pass resumes with a profile already in the store");
    assert!(third.resumed, "and it says so");
}

/// The canonical Amoru job: a kernel that appends a column. Its `output_schema` reports its
/// input unchanged, as an opaque kernel's must, so the sink is opened with a schema that is
/// not the one it is written with (08 f.1).
#[test]
fn rt_t7_a_kernel_that_appends_a_column() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t7");
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    let rows: u64 = 20_000;
    write_parquet(&input, rows, 2);

    let kernel: Arc<dyn Kernel> = Arc::new(support::Appender::new("loud"));
    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            amoru_sources::ParquetSource::new(
                amoru_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn amoru_kernel::Source>)
        })),
        vec![kernel],
        SinkSpec::Build(Box::new(move |ctx| {
            amoru_sinks::ParquetSink::new(
                amoru_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn amoru_kernel::Sink>)
        })),
    );
    spec.budget = Some(512 << 20);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");

    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!("a kernel that appends a column must run: {error}"),
    };
    assert_eq!(report.exit, amoru_runtime::ExitReason::Completed);
    assert_eq!(read_back_rows(&out_dir), rows, "every row is written");
    assert!(
        support::output_has_column(&out_dir, "loud"),
        "the appended column reached the files"
    );
}

/// The spec both halves of the resume test use.
fn resume_spec(
    input: &std::path::Path,
    out_dir: &std::path::Path,
    staging: &std::path::Path,
    keep: bool,
    resume: Option<std::path::PathBuf>,
    kernels: Vec<Arc<dyn Kernel>>,
) -> RunSpec {
    let source_path = input.to_path_buf();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            amoru_sources::ParquetSource::new(
                amoru_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn amoru_kernel::Source>)
        })),
        kernels,
        SinkSpec::Build(Box::new(move |ctx| {
            amoru_sinks::ParquetSink::new(
                amoru_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    row_group_bytes: 2 << 20,
                    file_bytes: 8 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn amoru_kernel::Sink>)
        })),
    );
    spec.budget = Some(1 << 30);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(staging.to_path_buf());
    spec.staging_limit = Some(1 << 30);
    spec.checkpoint_keep = keep;
    spec.profiles_dir = Some(staging.join("profiles"));
    spec.checkpoint_interval_ms = 500;
    spec.resume = resume;
    spec
}

/// Twenty runs in one process leave no arena behind. AR-I3, 12 g.
///
/// A user's notebook or a service calls `run` repeatedly in one process, so an arena whose
/// mapping is not returned at the end of a run is a defect in the usage pattern we most
/// expect. It was one: `Buffer::split_at` and `Buffer::into_arrow_buffer` consume the buffer
/// through `ManuallyDrop` and cloned the arena token beside the field nothing would ever drop,
/// so one token leaked per morsel, the region stayed mapped for the life of the process, and a
/// suite of runs at a large ceiling was killed by the host.
#[test]
fn rt_t8_twenty_runs_leave_no_arena_behind() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t8");
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, 20_000, 2);
    const CEILING: u64 = 512 << 20;
    const RUNS: usize = 20;

    let mut after_first = None;
    for run in 0..RUNS {
        let out_dir = scratch.path().join(format!("out-{run}"));
        std::fs::create_dir_all(&out_dir).expect("the output directory");
        let source_path = input.clone();
        let sink_url = format!("file://{}", out_dir.display());
        let kernel: Arc<dyn Kernel> = Arc::new(Doubler::new());
        let mut spec = RunSpec::new(
            SourceSpec::Build(Box::new(move |ctx| {
                amoru_sources::ParquetSource::new(
                    amoru_sources::ParquetSourceConfig {
                        urls: vec![format!("file://{}", source_path.display())],
                        ..Default::default()
                    },
                    ctx.reactor.clone(),
                    ctx.object_metadata()?,
                )
                .map(|source| Arc::new(source) as Arc<dyn amoru_kernel::Source>)
            })),
            vec![kernel],
            SinkSpec::Build(Box::new(move |ctx| {
                amoru_sinks::ParquetSink::new(
                    amoru_sinks::ParquetSinkConfig {
                        url: sink_url.clone(),
                        row_group_bytes: 1 << 20,
                        file_bytes: 4 << 20,
                        ..Default::default()
                    },
                    ctx.reactor.clone(),
                    ctx.alloc.clone(),
                )
                .map(|sink| Box::new(sink) as Box<dyn amoru_kernel::Sink>)
            })),
        );
        spec.budget = Some(CEILING);
        spec.cpu = Some(2.0);
        spec.staging_dir = Some(scratch.path().join("staging"));
        spec.profiles_dir = Some(scratch.path().join("profiles"));
        std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");

        let report = match Runtime::run(spec, CancelToken::new()) {
            Ok(report) => report,
            Err(error) => panic!("run {run} of {RUNS} did not complete: {error}"),
        };
        assert_eq!(report.exit, amoru_runtime::ExitReason::Completed);

        if let Some(rss) = support::resident_bytes() {
            let first = *after_first.get_or_insert(rss);
            // One leaked arena is a whole ceiling, so a bound of one ceiling above the first
            // run's figure catches the defect and tolerates an allocator's own high water.
            assert!(
                rss <= first + CEILING,
                "run {run} of {RUNS}: resident {rss} bytes against {first} after the first, \
                 which is more than one {CEILING} byte arena: a run is keeping its region"
            );
        }
    }
}
