//! A job document, version 2 (SDD 22 d.4), that runs an image `moruna build` made (SDD 30):
//! Parquet in, the image's command as the one kernel, Parquet out. The three tasks and the two
//! edges between them are the DAG; the image is named by the OCI layout directory and the
//! reference `moruna build` printed.
//!
//! ```text
//! cargo run -p moruna-job --example image_job -- \
//!     --layout moruna-image --reference add_doubled:latest \
//!     --input in.parquet --output out --work work --job job.mjb
//! moruna run job.mjb --sandbox-cgroup /sys/fs/cgroup/moruna
//! ```
//!
//! The document is checked against the standard kinds before it is written, so a mistake is
//! refused here, naming its field, and not by the run.

use std::path::PathBuf;
use std::process::ExitCode;

use moruna_job::{BudgetDoc, Catalogue, Edge, Job, KindDoc, StagingDoc, Task, check};
use serde_json::json;

/// What the document is made from.
#[derive(Debug, Default)]
struct Args {
    layout: String,
    reference: String,
    input: String,
    output: String,
    work: String,
    job: PathBuf,
}

const MEMORY_BYTES: u64 = 1 << 30;
const STAGING_BYTES: u64 = 1 << 30;

fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut found = Args::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--layout" => found.layout = value,
            "--reference" => found.reference = value,
            "--input" => found.input = value,
            "--output" => found.output = value,
            "--work" => found.work = value,
            "--job" => found.job = PathBuf::from(value),
            other => return Err(format!("`{other}` is not an option")),
        }
    }
    for (name, value) in [
        ("--layout", found.layout.as_str()),
        ("--reference", found.reference.as_str()),
        ("--input", found.input.as_str()),
        ("--output", found.output.as_str()),
        ("--work", found.work.as_str()),
        ("--job", found.job.to_str().unwrap_or("")),
    ] {
        if value.is_empty() {
            return Err(format!("{name} is required"));
        }
    }
    Ok(found)
}

fn entry(value: serde_json::Value) -> Result<KindDoc, String> {
    KindDoc::from_value(value)
}

/// The job: `in` (Parquet) to `image` (the image kind) to `out` (Parquet). The unpacked root of
/// the image is cached under `<work>/images`, the run's staging and profiles under `<work>`.
fn document(args: &Args) -> Result<Job, String> {
    let image = entry(json!({
        "kind": "image",
        "layout": args.layout,
        "reference": args.reference,
        "cache": format!("{}/images", args.work),
    }))?;
    let mut job = Job::new(
        vec![
            Task::source(
                "in",
                entry(json!({"kind": "parquet", "url": format!("file://{}", args.input)}))?,
            ),
            Task::kernel("image", image),
            Task::sink(
                "out",
                entry(json!({"kind": "parquet", "url": format!("file://{}", args.output)}))?,
            ),
        ],
        vec![Edge::between("in", "image"), Edge::between("image", "out")],
    );
    job.budget = BudgetDoc {
        memory_bytes: Some(MEMORY_BYTES),
        cpu: Some(2.0),
        elastic: None,
    };
    job.staging = StagingDoc {
        dir: Some(format!("{}/staging", args.work)),
        limit_bytes: Some(STAGING_BYTES),
        ..StagingDoc::default()
    };
    job.profiles_dir = Some(format!("{}/profiles", args.work));
    Ok(job)
}

fn write(args: &Args) -> Result<(), String> {
    let job = document(args)?;
    let catalogue = Catalogue::standard().map_err(|e| e.to_string())?;
    check(&job, &catalogue).map_err(|refusals| {
        refusals
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    // The run writes into these; they are the document's to say and the example's to make.
    for dir in [
        format!("{}/staging", args.work),
        format!("{}/profiles", args.work),
        args.output.clone(),
    ] {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    }
    let bytes = job.to_flatbuffer().map_err(|e| e.to_string())?;
    std::fs::write(&args.job, bytes).map_err(|e| format!("{}: {e}", args.job.display()))
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)).and_then(|args| write(&args).map(|()| args)) {
        Ok(args) => {
            println!("{}", args.job.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("image_job: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_job::{JOB_VERSION, Role};

    fn args() -> Args {
        parse(
            [
                "--layout",
                "/l",
                "--reference",
                "f:latest",
                "--input",
                "/in.parquet",
                "--output",
                "/out",
                "--work",
                "/w",
                "--job",
                "/j.mjb",
            ]
            .map(String::from),
        )
        .expect("every option is given")
    }

    #[test]
    fn the_document_is_a_dag_of_three_tasks_and_passes_the_check() {
        let job = document(&args()).expect("a document");
        let ids: Vec<&str> = job.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["in", "image", "out"]);
        assert_eq!(job.tasks[1].role, Role::Kernel);
        assert_eq!(job.tasks[1].kind, "image");
        assert_eq!(job.tasks[1].settings["layout"], "/l");
        assert_eq!(job.tasks[1].settings["reference"], "f:latest");
        assert_eq!(job.edges.len(), 2);
        check(&job, &Catalogue::standard().unwrap()).expect("the standard kinds accept it");
    }

    #[test]
    fn the_bytes_are_version_two_and_read_back_the_same() {
        let job = document(&args()).unwrap();
        let bytes = job.to_flatbuffer().unwrap();
        assert!(moruna_job::is_job_flatbuffer(&bytes));
        let back = Job::from_flatbuffer(&bytes).unwrap();
        assert_eq!(back.tasks[1].settings, job.tasks[1].settings);
        assert_eq!(JOB_VERSION, 2);
    }

    #[test]
    fn a_missing_or_unknown_option_is_refused_by_name() {
        let missing = parse(["--layout".to_string(), "/l".to_string()]).unwrap_err();
        assert_eq!(missing, "--reference is required");
        let unknown = parse(["--bogus".to_string(), "1".to_string()]).unwrap_err();
        assert!(unknown.contains("--bogus"), "{unknown}");
        let valueless = parse(["--layout".to_string()]).unwrap_err();
        assert_eq!(valueless, "--layout needs a value");
    }

    #[test]
    fn a_good_document_is_written_to_the_job_path() {
        let dir = std::env::temp_dir().join(format!("image_job-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut found = args();
        found.job = dir.join("job.mjb");
        found.work = dir.join("work").display().to_string();
        found.output = dir.join("out").display().to_string();
        write(&found).expect("the standard kinds accept it, so it is written");
        assert!(moruna_job::is_job_flatbuffer(
            &std::fs::read(&found.job).unwrap()
        ));
        for made in ["work/staging", "work/profiles", "out"] {
            assert!(dir.join(made).is_dir(), "{made} is made");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
