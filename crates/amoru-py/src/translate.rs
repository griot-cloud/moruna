//! `amoru.run` argument translation and its range clamping (f.3, PY-I10).
//!
//! Every user argument is range checked exactly once, here. A value outside the range its row of
//! the preamble's configuration table gives is clamped to the nearest bound and a note
//! `clamped <name> from <given> to <bound>` is appended to the notes the run report carries; a
//! value of the wrong shape (an unknown `on_error`, an unknown `sizer`, a non-boolean for a row
//! whose range is `fixed`) is a `Config` error and is not clamped. `budget` and `cpu` go to
//! discovery unchanged, which owns their clamping against the discovered ceiling; the scheduler
//! clamps its own knobs (SC f.15). Nothing else clamps anything.

use std::path::{Path, PathBuf};

use amoru_kernel::{AmoruError, ErrorPolicy, Result, SizerKind};

use crate::handles::{SinkSpec, SourceSpec};
use crate::size::parse_size;

/// `checkpoint.interval_ms` (preamble section 5).
pub const CHECKPOINT_INTERVAL_MS: (u64, u64) = (500, 60_000);
/// `sink.row_group_bytes` (preamble section 5).
pub const ROW_GROUP_BYTES: (u64, u64) = (16 << 20, 1 << 30);
/// `sink.file_bytes` (preamble section 5).
pub const FILE_BYTES: (u64, u64) = (64 << 20, 16u64 << 30);
/// `budget.disk`: the upper bound is the free space in the staging directory, which component 9
/// resolves; the surface holds the lower bound, which is the only one it can know (preamble
/// section 5, `0 .. free`).
pub const STAGING_LIMIT_MIN: u64 = 0;

/// A size argument as Python passed it: bytes, or a string for the parser of 03 f.3.
#[derive(Clone, Debug)]
pub enum SizeArg {
    /// An integer number of bytes.
    Bytes(u64),
    /// A string such as `"6GiB"`.
    Text(String),
}

impl SizeArg {
    /// Resolve to bytes, naming the argument in any error.
    pub fn bytes(&self, name: &'static str) -> Result<u64> {
        match self {
            SizeArg::Bytes(b) => Ok(*b),
            SizeArg::Text(s) => parse_size(name, s),
        }
    }
}

/// `on_error=` as Python passed it.
#[derive(Clone, Debug)]
pub enum OnErrorArg {
    /// `"terminate"` or `"skip"`, or anything else, which is refused.
    Name(String),
    /// `("budget", n)`.
    Budget(u32),
}

/// `resume=` after the surface resolved its shape, before any manifest is read (f.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeArg {
    /// `"auto"`: the newest manifest under the staging directory.
    Auto,
    /// A run id: 32 lowercase hexadecimal characters.
    RunId(String),
    /// A path to a `manifest.json`.
    Path(PathBuf),
}

/// Everything `amoru.run` was given, before translation.
pub struct RawArgs {
    /// `budget=`.
    pub budget: Option<SizeArg>,
    /// `cpu=`.
    pub cpu: Option<f64>,
    /// `trace=`.
    pub trace: Option<String>,
    /// `staging_dir=`.
    pub staging_dir: Option<String>,
    /// `staging_limit=`.
    pub staging_limit: Option<SizeArg>,
    /// `on_error=`.
    pub on_error: OnErrorArg,
    /// `ordered=`.
    pub ordered: bool,
    /// `sizer=`.
    pub sizer: String,
    /// `profiles_dir=`.
    pub profiles_dir: Option<String>,
    /// `allow_gil=`.
    pub allow_gil: bool,
    /// `checkpoint=`.
    pub checkpoint: bool,
    /// `checkpoint_interval=`, in seconds.
    pub checkpoint_interval: f64,
    /// `keep_checkpoint=`.
    pub keep_checkpoint: bool,
    /// `resume=`.
    pub resume: Option<String>,
}

/// The translated run arguments: what the facade's `RunSpec` is filled from (d.1), plus the
/// notes the clamping produced.
pub struct Translated {
    /// `budget=`, unclamped: discovery owns the ceiling.
    pub budget: Option<u64>,
    /// `cpu=`, unclamped: discovery owns the ceiling.
    pub cpu: Option<f64>,
    /// `trace=`, a file or a directory, validated to be writable.
    pub trace_path: Option<PathBuf>,
    /// `staging_dir=`.
    pub staging_dir: Option<PathBuf>,
    /// `staging_limit=`, clamped to `budget.disk`'s lower bound.
    pub staging_limit: Option<u64>,
    /// `on_error=`.
    pub error_policy: ErrorPolicy,
    /// `ordered=`.
    pub ordered: bool,
    /// `sizer=`.
    pub sizer: SizerKind,
    /// `profiles_dir=`.
    pub profiles_dir: Option<PathBuf>,
    /// `allow_gil=`.
    pub allow_gil: bool,
    /// `checkpoint=`.
    pub checkpoint: bool,
    /// `checkpoint_interval=`, in milliseconds, clamped to `checkpoint.interval_ms`.
    pub checkpoint_interval_ms: u64,
    /// `keep_checkpoint=`.
    pub checkpoint_keep: bool,
    /// `resume=`.
    pub resume: Option<ResumeArg>,
    /// Every clamp this translation made, in the order it made them (PY-I10).
    pub notes: Vec<String>,
}

/// Clamp `value` into `[lo, hi]`, appending the note PY-I10 requires when it moves.
pub fn clamp(name: &str, value: u64, bounds: (u64, u64), notes: &mut Vec<String>) -> u64 {
    let (lo, hi) = bounds;
    if value < lo {
        notes.push(format!("clamped {name} from {value} to {lo}"));
        lo
    } else if value > hi {
        notes.push(format!("clamped {name} from {value} to {hi}"));
        hi
    } else {
        value
    }
}

/// `sink.row_group_bytes`, clamped where the `ParquetSink` handle is constructed.
pub fn clamp_row_group_bytes(value: u64, notes: &mut Vec<String>) -> u64 {
    clamp("sink.row_group_bytes", value, ROW_GROUP_BYTES, notes)
}

/// `sink.file_bytes`, clamped where the sink handle is constructed.
pub fn clamp_file_bytes(value: u64, notes: &mut Vec<String>) -> u64 {
    clamp("sink.file_bytes", value, FILE_BYTES, notes)
}

/// `on_error` to `ErrorPolicy` (contracts d.11). An unknown name is a `Config` error, never a
/// clamp (f.3).
pub fn error_policy(arg: &OnErrorArg) -> Result<ErrorPolicy> {
    match arg {
        OnErrorArg::Budget(n) => Ok(ErrorPolicy::Budget(*n)),
        OnErrorArg::Name(name) => match name.as_str() {
            "terminate" => Ok(ErrorPolicy::Terminate),
            "skip" => Ok(ErrorPolicy::Skip),
            other => Err(config(
                "errors.policy",
                format!(
                    "unknown on_error `{other}` (use \"terminate\", \"skip\" or (\"budget\", n))"
                ),
            )),
        },
    }
}

/// `sizer` to `SizerKind`. An unknown name is a `Config` error, never a clamp (f.3).
pub fn sizer_kind(name: &str) -> Result<SizerKind> {
    match name {
        "rule" => Ok(SizerKind::Rule),
        "learned" => Ok(SizerKind::Learned),
        other => Err(config(
            "sizer",
            format!("unknown sizer `{other}` (use \"rule\" or \"learned\")"),
        )),
    }
}

/// `resume=` to its three shapes (f.7). A 32 character lowercase hexadecimal string is a run id;
/// `"auto"` is the newest manifest; anything else is a path.
pub fn resume_arg(value: &str) -> ResumeArg {
    if value == "auto" {
        return ResumeArg::Auto;
    }
    let is_run_id = value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if is_run_id {
        ResumeArg::RunId(value.to_string())
    } else {
        ResumeArg::Path(PathBuf::from(value))
    }
}

/// `trace=`'s path validity (f.3). A path whose directory does not exist cannot be clamped to a
/// bound, so it is refused by name rather than silently ignored; h decides the file name inside a
/// directory, and the facade does that once it has the run id.
pub fn trace_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    let dir: &Path = if path.is_dir() {
        &path
    } else {
        match path.parent() {
            Some(p) if p.as_os_str().is_empty() => Path::new("."),
            Some(p) => p,
            None => Path::new("."),
        }
    };
    if !dir.is_dir() {
        return Err(config(
            "trace.path",
            format!(
                "`{value}` is not a writable path: `{}` is not a directory",
                dir.display()
            ),
        ));
    }
    Ok(path)
}

/// Normalise a URL or path for the sink equals source rule (f.3): the scheme is lower cased, a
/// trailing slash is removed and a `file://` URL or a bare relative path becomes an absolute path.
pub fn normalise_target(value: &str) -> String {
    let trimmed = value.trim();
    let normalised = match trimmed.find("://") {
        Some(idx) => {
            let (scheme, rest) = trimmed.split_at(idx);
            let scheme = scheme.to_ascii_lowercase();
            if scheme == "file" {
                absolute(rest.trim_start_matches("://"))
            } else {
                format!("{scheme}{rest}")
            }
        }
        None => absolute(trimmed),
    };
    let stripped = normalised.trim_end_matches('/');
    if stripped.is_empty() {
        "/".to_string()
    } else {
        stripped.to_string()
    }
}

fn absolute(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return p.to_string_lossy().into_owned();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().into_owned(),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// The sink equals source rule (f.3, PY-T14): the run is refused with `Plan` when the sink writes
/// where a source reads, or into a directory a source reads from.
pub fn check_sink_not_source(sink: &SinkSpec, source: &SourceSpec) -> Result<()> {
    let sink_target = normalise_target(&sink.target());
    let sink_prefix = format!("{sink_target}/");
    for raw in source.targets() {
        let src = normalise_target(&raw);
        if src == sink_target || src.starts_with(&sink_prefix) {
            return Err(AmoruError::Plan(format!(
                "the sink writes where the source reads: sink `{}` and source `{}` resolve to \
                 `{sink_target}` and `{src}`; the run would overwrite or write into its input",
                sink.target(),
                raw
            )));
        }
    }
    Ok(())
}

/// Translate the arguments of `amoru.run` (f.3). `sink_notes` are the clamps the sink handle's
/// own constructor already made; they come first, because they happened first.
pub fn translate(raw: RawArgs, sink_notes: Vec<String>) -> Result<Translated> {
    let mut notes = sink_notes;

    let budget = match &raw.budget {
        Some(arg) => Some(arg.bytes("budget.host")?),
        None => None,
    };
    let staging_limit = match &raw.staging_limit {
        Some(arg) => {
            let given = arg.bytes("budget.disk")?;
            Some(clamp(
                "budget.disk",
                given,
                (STAGING_LIMIT_MIN, u64::MAX),
                &mut notes,
            ))
        }
        None => None,
    };
    let interval_ms_given = raw.checkpoint_interval * 1000.0;
    if !interval_ms_given.is_finite() || interval_ms_given < 0.0 {
        return Err(config(
            "checkpoint.interval_ms",
            format!("`{}` is not a number of seconds", raw.checkpoint_interval),
        ));
    }
    let checkpoint_interval_ms = clamp(
        "checkpoint.interval_ms",
        interval_ms_given.round() as u64,
        CHECKPOINT_INTERVAL_MS,
        &mut notes,
    );

    Ok(Translated {
        budget,
        cpu: raw.cpu,
        trace_path: match &raw.trace {
            Some(value) => Some(trace_path(value)?),
            None => None,
        },
        staging_dir: raw.staging_dir.as_deref().map(PathBuf::from),
        staging_limit,
        error_policy: error_policy(&raw.on_error)?,
        ordered: raw.ordered,
        sizer: sizer_kind(&raw.sizer)?,
        profiles_dir: raw.profiles_dir.as_deref().map(PathBuf::from),
        allow_gil: raw.allow_gil,
        checkpoint: raw.checkpoint,
        checkpoint_interval_ms,
        checkpoint_keep: raw.keep_checkpoint,
        resume: raw.resume.as_deref().map(resume_arg),
        notes,
    })
}

fn config(name: &'static str, msg: String) -> AmoruError {
    AmoruError::Config { name, msg }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amoru_sinks::{ParquetSinkConfig, TensorSinkConfig};
    use amoru_sources::{ParquetSourceConfig, TensorSourceConfig};

    fn defaults() -> RawArgs {
        RawArgs {
            budget: None,
            cpu: None,
            trace: None,
            staging_dir: None,
            staging_limit: None,
            on_error: OnErrorArg::Name("terminate".into()),
            ordered: false,
            sizer: "rule".into(),
            profiles_dir: None,
            allow_gil: false,
            checkpoint: true,
            checkpoint_interval: 5.0,
            keep_checkpoint: false,
            resume: None,
        }
    }

    fn parquet_sink(url: &str) -> SinkSpec {
        SinkSpec::Parquet(ParquetSinkConfig {
            url: url.to_string(),
            row_group_bytes: 128 << 20,
            file_bytes: 1 << 30,
            compression: parquet::basic::Compression::UNCOMPRESSED,
            writer_props: None,
        })
    }

    fn parquet_source(urls: &[&str]) -> SourceSpec {
        SourceSpec::Parquet(ParquetSourceConfig {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            columns: None,
            filters: Vec::new(),
            batch_rows_hint: None,
        })
    }

    /// PY-T13 configuration_clamping: one value below and one above every row of the preamble's
    /// configuration table whose owner is `user` and whose range is not `fixed`, through the
    /// arguments of `amoru.run`; each arrives at the bound and exactly one note names the clamp.
    #[test]
    fn py_t13_configuration_clamping() {
        // checkpoint.interval_ms, 500 .. 60000.
        let low = translate(
            RawArgs {
                checkpoint_interval: 0.1,
                ..defaults()
            },
            Vec::new(),
        )
        .expect("low interval");
        assert_eq!(low.checkpoint_interval_ms, 500);
        assert_eq!(
            low.notes,
            vec!["clamped checkpoint.interval_ms from 100 to 500".to_string()]
        );
        let high = translate(
            RawArgs {
                checkpoint_interval: 600.0,
                ..defaults()
            },
            Vec::new(),
        )
        .expect("high interval");
        assert_eq!(high.checkpoint_interval_ms, 60_000);
        assert_eq!(high.notes.len(), 1);
        let inside = translate(defaults(), Vec::new()).expect("default interval");
        assert_eq!(inside.checkpoint_interval_ms, 5_000);
        assert!(inside.notes.is_empty());

        // sink.row_group_bytes, 16 MiB .. 1 GiB, and sink.file_bytes, 64 MiB .. 16 GiB: clamped
        // where the sink handle is built, reported in the same note list.
        let mut notes = Vec::new();
        assert_eq!(clamp_row_group_bytes(1 << 20, &mut notes), 16 << 20);
        assert_eq!(clamp_row_group_bytes(4u64 << 30, &mut notes), 1 << 30);
        assert_eq!(clamp_file_bytes(1 << 20, &mut notes), 64 << 20);
        assert_eq!(clamp_file_bytes(64u64 << 30, &mut notes), 16u64 << 30);
        assert_eq!(notes.len(), 4);
        assert!(notes[0].starts_with("clamped sink.row_group_bytes from 1048576 to "));
        let carried = translate(defaults(), notes.clone()).expect("sink notes carried");
        assert_eq!(carried.notes, notes);

        // budget.disk, 0 .. free: the lower bound is the surface's, the upper is component 9's.
        let disk = translate(
            RawArgs {
                staging_limit: Some(SizeArg::Bytes(0)),
                ..defaults()
            },
            Vec::new(),
        )
        .expect("staging limit");
        assert_eq!(disk.staging_limit, Some(0));
        assert!(disk.notes.is_empty());

        // budget.host and cpu reach discovery unclamped: it owns the discovered ceiling.
        let budget = translate(
            RawArgs {
                budget: Some(SizeArg::Text("6GiB".into())),
                cpu: Some(1_000.0),
                ..defaults()
            },
            Vec::new(),
        )
        .expect("budget");
        assert_eq!(budget.budget, Some(6 << 30));
        assert_eq!(budget.cpu, Some(1_000.0));
        assert!(budget.notes.is_empty());

        // A row whose range is an enumeration refuses a foreign value rather than clamping it.
        let bad_policy = translate(
            RawArgs {
                on_error: OnErrorArg::Name("explode".into()),
                ..defaults()
            },
            Vec::new(),
        );
        assert!(matches!(
            bad_policy,
            Err(AmoruError::Config {
                name: "errors.policy",
                ..
            })
        ));
        let bad_sizer = translate(
            RawArgs {
                sizer: "psychic".into(),
                ..defaults()
            },
            Vec::new(),
        );
        assert!(matches!(
            bad_sizer,
            Err(AmoruError::Config { name: "sizer", .. })
        ));
    }

    /// PY-T14 sink_equals_source (f.3).
    #[test]
    fn py_t14_sink_equals_source() {
        assert!(
            check_sink_not_source(
                &parquet_sink("s3://b/in/"),
                &parquet_source(&["s3://b/in/"])
            )
            .is_err()
        );
        assert!(
            check_sink_not_source(&parquet_sink("s3://b/"), &parquet_source(&["s3://b/in/"]))
                .is_err()
        );
        assert!(
            check_sink_not_source(&parquet_sink("S3://B/in"), &parquet_source(&["s3://B/in/"]))
                .is_err()
        );
        assert!(
            check_sink_not_source(
                &parquet_sink("s3://b/in2/"),
                &parquet_source(&["s3://b/in/"])
            )
            .is_ok()
        );
        assert!(
            check_sink_not_source(
                &parquet_sink("s3://other/in/"),
                &parquet_source(&["s3://b/in/"])
            )
            .is_ok()
        );

        let file_sink = parquet_sink("file:///data/out");
        let file_source = parquet_source(&["/data/out/part.parquet"]);
        let err = check_sink_not_source(&file_sink, &file_source).expect_err("prefix");
        assert!(matches!(err, AmoruError::Plan(_)), "{err}");
        assert!(
            check_sink_not_source(&file_sink, &parquet_source(&["/data/in/part.parquet"])).is_ok()
        );

        // A tensor sink and a tensor source compare as paths.
        let tensor_sink = SinkSpec::Tensor(TensorSinkConfig {
            path: PathBuf::from("/data/t"),
            format: amoru_sinks::TensorFormat::Amb1,
            one_file_per_morsel: false,
            name: "tensor".into(),
        });
        let tensor_source = SourceSpec::Tensor(TensorSourceConfig {
            paths: vec![PathBuf::from("/data/t/weights.safetensors")],
            tensors: None,
            slice_rows_hint: None,
        });
        assert!(check_sink_not_source(&tensor_sink, &tensor_source).is_err());
        assert_eq!(tensor_sink.kind_name(), "TensorSink");

        // An iterator source reads no URL, so nothing can collide with it.
        let iter = SourceSpec::Iterator {
            schema: crate::handles::IteratorSchema::Tensor {
                dtype: "f32".into(),
                shape: vec![-1, 4],
            },
        };
        assert!(check_sink_not_source(&parquet_sink("s3://b/out/"), &iter).is_ok());
    }

    #[test]
    fn resume_shapes() {
        assert_eq!(resume_arg("auto"), ResumeArg::Auto);
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(resume_arg(id), ResumeArg::RunId(id.to_string()));
        assert_eq!(
            resume_arg("/tmp/run/manifest.json"),
            ResumeArg::Path(PathBuf::from("/tmp/run/manifest.json"))
        );
        // 32 characters that are not hexadecimal are a path, not a run id.
        assert!(matches!(resume_arg(&"z".repeat(32)), ResumeArg::Path(_)));
    }

    #[test]
    fn trace_path_validity() {
        let dir = std::env::temp_dir();
        assert_eq!(
            trace_path(&dir.to_string_lossy()).expect("a directory"),
            dir
        );
        let file = dir.join(format!("amoru-py-trace-{}.arrow", std::process::id()));
        assert_eq!(trace_path(&file.to_string_lossy()).expect("a file"), file);
        let missing = dir.join("no-such-amoru-dir").join("t.arrow");
        assert!(matches!(
            trace_path(&missing.to_string_lossy()),
            Err(AmoruError::Config {
                name: "trace.path",
                ..
            })
        ));
    }

    #[test]
    fn error_policies_and_sizers() {
        assert_eq!(
            error_policy(&OnErrorArg::Name("skip".into())).expect("skip"),
            ErrorPolicy::Skip
        );
        assert_eq!(
            error_policy(&OnErrorArg::Budget(7)).expect("budget"),
            ErrorPolicy::Budget(7)
        );
        assert_eq!(sizer_kind("learned").expect("learned"), SizerKind::Learned);
    }

    #[test]
    fn sizes_and_paths_translate() {
        let t = translate(
            RawArgs {
                staging_dir: Some("/tmp/amoru-staging".into()),
                profiles_dir: Some("/tmp/amoru-profiles".into()),
                staging_limit: Some(SizeArg::Text("2GiB".into())),
                ordered: true,
                allow_gil: true,
                checkpoint: false,
                keep_checkpoint: true,
                resume: Some("auto".into()),
                ..defaults()
            },
            Vec::new(),
        )
        .expect("translate");
        assert_eq!(t.staging_dir, Some(PathBuf::from("/tmp/amoru-staging")));
        assert_eq!(t.profiles_dir, Some(PathBuf::from("/tmp/amoru-profiles")));
        assert_eq!(t.staging_limit, Some(2 << 30));
        assert!(t.ordered && t.allow_gil && t.checkpoint_keep && !t.checkpoint);
        assert_eq!(t.resume, Some(ResumeArg::Auto));
        assert!(matches!(
            translate(
                RawArgs {
                    checkpoint_interval: f64::NAN,
                    ..defaults()
                },
                Vec::new()
            ),
            Err(AmoruError::Config {
                name: "checkpoint.interval_ms",
                ..
            })
        ));
        assert_eq!(normalise_target("s3://B/x/"), "s3://B/x");
        assert_eq!(normalise_target("file:///"), "/");
    }
}
