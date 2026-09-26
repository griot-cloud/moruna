//! The range checks and translations every description of a run passes through once (12 f.3,
//! PY-I10, MH 4.1).
//!
//! These were the Python surface's; they are here because a document from a file and the
//! arguments of `moruna.run` are two spellings of one run, and a clamp or a refusal that only
//! one of them applied would make them two runs. `moruna-py` re-exports them under their old
//! names.

use std::path::{Path, PathBuf};

use moruna_kernel::{MorunaError, Result, RunId};
use moruna_placement::PlacementEngine;

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
/// The default `sink.row_group_bytes`.
pub const DEFAULT_ROW_GROUP_BYTES: u64 = 128 << 20;
/// The default `sink.file_bytes`.
pub const DEFAULT_FILE_BYTES: u64 = 1 << 30;

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

/// `sink.row_group_bytes`, clamped.
pub fn clamp_row_group_bytes(value: u64, notes: &mut Vec<String>) -> u64 {
    clamp("sink.row_group_bytes", value, ROW_GROUP_BYTES, notes)
}

/// `sink.file_bytes`, clamped.
pub fn clamp_file_bytes(value: u64, notes: &mut Vec<String>) -> u64 {
    clamp("sink.file_bytes", value, FILE_BYTES, notes)
}

/// `resume` after its shape is known, before any manifest is read (12 f.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeArg {
    /// `"auto"`: the newest manifest under the staging directory.
    Auto,
    /// A run id: 32 lowercase hexadecimal characters.
    RunId(String),
    /// A path to a `manifest.json`.
    Path(PathBuf),
}

/// `resume` to its three shapes (12 f.7). A 32 character lowercase hexadecimal string is a run
/// id; `"auto"` is the newest manifest; anything else is a path.
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

/// Resolve `resume` to a manifest path before anything is built (12 f.7). `"auto"` and a run id
/// are looked up under the staging directory; a path is taken as given.
pub fn resolve_resume(
    arg: Option<&ResumeArg>,
    staging_dir: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some(arg) = arg else {
        return Ok(None);
    };
    match arg {
        ResumeArg::Path(p) => Ok(Some(p.clone())),
        ResumeArg::Auto | ResumeArg::RunId(_) => {
            let dir = staging_dir.ok_or_else(|| {
                MorunaError::Resume(
                    "resume= by run id or \"auto\" needs staging_dir= so the manifest can be \
                     found; pass the manifest path instead"
                        .into(),
                )
            })?;
            let id = match arg {
                ResumeArg::RunId(hex) => Some(RunId::from_hex(hex).ok_or_else(|| {
                    MorunaError::Resume(format!(
                        "`{hex}` is not a run id: 32 hex characters are needed"
                    ))
                })?),
                _ => None,
            };
            match PlacementEngine::find_manifest(dir, id)? {
                Some(path) => Ok(Some(path)),
                None => Err(MorunaError::Resume(format!(
                    "no manifest to resume from under `{}`",
                    dir.display()
                ))),
            }
        }
    }
}

/// `trace`'s path validity (12 f.3). A path whose directory does not exist cannot be clamped to
/// a bound, so it is refused by name rather than silently ignored; 12 h decides the file name
/// inside a directory, and the facade does that once it has the run id.
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
        return Err(MorunaError::Config {
            name: "trace.path",
            msg: format!(
                "`{value}` is not a writable path: `{}` is not a directory",
                dir.display()
            ),
        });
    }
    Ok(path)
}

/// A sink URL with a scheme, from what the user wrote (12 f.3). A bare path is a local
/// directory and becomes an absolute `file://` URL; a string with a scheme is untouched.
pub fn local_url(url: &str) -> String {
    if url.contains("://") {
        return url.to_string();
    }
    format!("file://{}", absolute(url))
}

/// A local path from a `file://` URL or a bare path, for the sinks and sources that take paths.
pub fn local_path(url: &str) -> PathBuf {
    PathBuf::from(url.strip_prefix("file://").unwrap_or(url))
}

/// Normalise a URL or path for the sink equals source rule (12 f.3): the scheme is lower cased,
/// a trailing slash is removed and a `file://` URL or a bare relative path becomes an absolute
/// path.
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

/// The sink equals source rule (12 f.3, PY-T14): refused with `Plan` when the sink writes where
/// a source reads, or into a directory a source reads from.
pub fn check_sink_not_source(sink: &str, sources: &[String]) -> Result<()> {
    let sink_target = normalise_target(sink);
    let sink_prefix = format!("{sink_target}/");
    for raw in sources {
        let src = normalise_target(raw);
        if src == sink_target || src.starts_with(&sink_prefix) {
            return Err(MorunaError::Plan(format!(
                "the sink writes where the source reads: sink `{sink}` and source `{raw}` \
                 resolve to `{sink_target}` and `{src}`; the run would overwrite or write into \
                 its input"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_name_themselves() {
        let mut notes = Vec::new();
        assert_eq!(clamp_row_group_bytes(1 << 20, &mut notes), 16 << 20);
        assert_eq!(clamp_row_group_bytes(4u64 << 30, &mut notes), 1 << 30);
        assert_eq!(clamp_file_bytes(1 << 20, &mut notes), 64 << 20);
        assert_eq!(clamp_file_bytes(64u64 << 30, &mut notes), 16u64 << 30);
        assert_eq!(clamp_file_bytes(1 << 30, &mut notes), 1 << 30);
        assert_eq!(notes.len(), 4);
        assert_eq!(
            notes[0],
            "clamped sink.row_group_bytes from 1048576 to 16777216"
        );
    }

    #[test]
    fn resume_shapes_and_resolution() {
        assert_eq!(resume_arg("auto"), ResumeArg::Auto);
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(resume_arg(id), ResumeArg::RunId(id.to_string()));
        assert!(matches!(resume_arg(&"z".repeat(32)), ResumeArg::Path(_)));

        assert_eq!(resolve_resume(None, None).expect("none"), None);
        let path = ResumeArg::Path(PathBuf::from("/tmp/m.json"));
        assert_eq!(
            resolve_resume(Some(&path), None).expect("a path"),
            Some(PathBuf::from("/tmp/m.json"))
        );
        assert!(matches!(
            resolve_resume(Some(&ResumeArg::Auto), None),
            Err(MorunaError::Resume(_))
        ));
        let empty =
            std::env::temp_dir().join(format!("moruna-resume-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).expect("dir");
        assert!(matches!(
            resolve_resume(Some(&ResumeArg::Auto), Some(&empty)),
            Err(MorunaError::Resume(_))
        ));
        assert!(matches!(
            resolve_resume(Some(&ResumeArg::RunId(id.into())), Some(&empty)),
            Err(MorunaError::Resume(_))
        ));
        assert!(matches!(
            resolve_resume(Some(&ResumeArg::RunId("xyz".into())), Some(&empty)),
            Err(MorunaError::Resume(_))
        ));
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn urls_and_paths() {
        assert_eq!(local_url("s3://b/out"), "s3://b/out");
        assert_eq!(local_url("/data/out"), "file:///data/out");
        assert!(local_url("rel/out").starts_with("file:///"));
        assert_eq!(local_path("file:///data/x"), PathBuf::from("/data/x"));
        assert_eq!(local_path("/data/x"), PathBuf::from("/data/x"));
        assert_eq!(normalise_target("s3://B/x/"), "s3://B/x");
        assert_eq!(normalise_target("file:///"), "/");
        assert!(check_sink_not_source("s3://b/", &["s3://b/in/".into()]).is_err());
        assert!(check_sink_not_source("s3://b/in2", &["s3://b/in/".into()]).is_ok());

        let dir = std::env::temp_dir();
        assert_eq!(trace_path(&dir.to_string_lossy()).expect("dir"), dir);
        assert!(trace_path("t.arrow").is_ok());
        assert!(matches!(
            trace_path("/no-such-moruna-dir/t.arrow"),
            Err(MorunaError::Config {
                name: "trace.path",
                ..
            })
        ));
    }
}
