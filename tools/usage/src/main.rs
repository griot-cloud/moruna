//! `moruna-usage`: the usage build's report (stream U, `architecture/data-os-streams.md`).
//!
//! ```text
//! moruna-usage report --src DIR --kit DIR [--prod-kit DIR] [--llvm DIR] --out DIR LABEL=RUN...
//! moruna-usage profraw --elf ELF --counters FILE --out FILE
//! ```
//!
//! `report` reads each job's evidence (a `MORUNA_USAGE_DIR` that `moruna run` filled: the
//! kernel's counters, the kernel ELF and the serial log with the programs' dumps), builds the
//! raw profiles, merges them per job and over all jobs with `llvm-profdata`, reads them back
//! with `llvm-cov export`, and writes `usage.md` and `usage.json` in OUT. `--src` is the tree
//! the usage build was built from, `--kit` its `build.sh` output, `--prod-kit` a production
//! build of the same tree for the sizes, and `--llvm` the kernel toolchain's `llvm-tools`
//! directory (else `MORUNA_LLVM_BIN`, else the active rustc's).
//!
//! `profraw` builds one raw profile from an ELF and its counter section's bytes.

mod graph;
mod llvm;
mod render;
mod report;
mod serial;
mod source;

use std::path::PathBuf;

use moruna_usage::{elf::Elf, profraw};

const USAGE: &str = "usage: moruna-usage report --src DIR --kit DIR [--prod-kit DIR] [--llvm DIR] --out DIR LABEL=RUN...\n       moruna-usage profraw --elf ELF --counters FILE --out FILE";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Report {
        src: PathBuf,
        kit: PathBuf,
        prod_kit: Option<PathBuf>,
        llvm: Option<PathBuf>,
        out: PathBuf,
        runs: Vec<(String, PathBuf)>,
    },
    Profraw {
        elf: PathBuf,
        counters: PathBuf,
        out: PathBuf,
    },
}

fn parse(args: &[String]) -> Result<Command, String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    let mut options: std::collections::BTreeMap<&str, PathBuf> = Default::default();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if let Some(name) = arg.strip_prefix("--") {
            let value = rest
                .get(i + 1)
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))?;
            if options.insert(name, PathBuf::from(value)).is_some() {
                return Err(format!("{arg} given twice"));
            }
            i += 2;
            continue;
        }
        let (label, dir) = arg
            .split_once('=')
            .filter(|(l, d)| !l.is_empty() && !d.is_empty() && !l.contains('/'))
            .ok_or_else(|| format!("{arg}: a job is LABEL=DIR\n{USAGE}"))?;
        if runs.iter().any(|(l, _)| l == label) {
            return Err(format!("job {label} given twice"));
        }
        runs.push((label.to_string(), PathBuf::from(dir)));
        i += 1;
    }
    let mut take = |name: &str| options.remove(name);
    let need =
        |v: Option<PathBuf>, name: &str| v.ok_or_else(|| format!("--{name} is needed\n{USAGE}"));
    let parsed = match command.as_str() {
        "report" => {
            let c = Command::Report {
                src: need(take("src"), "src")?,
                kit: need(take("kit"), "kit")?,
                prod_kit: take("prod-kit"),
                llvm: take("llvm"),
                out: need(take("out"), "out")?,
                runs: std::mem::take(&mut runs),
            };
            if let Command::Report { runs, .. } = &c
                && runs.is_empty()
            {
                return Err(format!("no jobs given\n{USAGE}"));
            }
            c
        }
        "profraw" => Command::Profraw {
            elf: need(take("elf"), "elf")?,
            counters: need(take("counters"), "counters")?,
            out: need(take("out"), "out")?,
        },
        other => return Err(format!("unknown command {other}\n{USAGE}")),
    };
    if let Some(name) = options.keys().next() {
        return Err(format!("--{name} is not an option of {command}\n{USAGE}"));
    }
    if !runs.is_empty() {
        return Err(format!("{command} takes no jobs\n{USAGE}"));
    }
    Ok(parsed)
}

fn run(command: Command, tools: Option<&dyn llvm::Tools>) -> Result<String, String> {
    match command {
        Command::Profraw { elf, counters, out } => {
            let bytes = std::fs::read(&elf).map_err(|e| format!("{}: {e}", elf.display()))?;
            let parsed = Elf::parse(&bytes).map_err(|e| format!("{}: {e}", elf.display()))?;
            let counters =
                std::fs::read(&counters).map_err(|e| format!("{}: {e}", counters.display()))?;
            let raw = profraw::build(&parsed, &counters).map_err(|e| e.to_string())?;
            std::fs::write(&out, raw).map_err(|e| format!("{}: {e}", out.display()))?;
            Ok(format!("wrote {}", out.display()))
        }
        Command::Report {
            src,
            kit,
            prod_kit,
            llvm,
            out,
            runs,
        } => {
            let found;
            let tools = match tools {
                Some(t) => t,
                None => {
                    found = llvm::Llvm {
                        bin: llvm::locate(llvm)?,
                    };
                    &found
                }
            };
            let cfg = report::Config {
                src: src.clone(),
                kit,
                prod_kit,
                out: out.clone(),
                runs,
            };
            let report = report::build(&cfg, tools)?;
            let table = std::fs::read_to_string(src.join("kernel/linux/core/src/table.rs"))
                .unwrap_or_default();
            let names = source::numbers(&table, "nr")
                .into_iter()
                .map(|(name, n)| (n, name.to_lowercase()))
                .collect();
            let json = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
            std::fs::write(out.join("usage.json"), json)
                .map_err(|e| format!("{}: {e}", out.display()))?;
            std::fs::write(out.join("usage.md"), render::markdown(&report, &names))
                .map_err(|e| format!("{}: {e}", out.display()))?;
            Ok(format!(
                "wrote {} and usage.json: {} jobs, {} candidates for removal, {} kept for the accelerator path",
                out.join("usage.md").display(),
                report.jobs.len(),
                report.unused.len(),
                report.keep.len()
            ))
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(|c| run(c, None)) {
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("moruna-usage: {why}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests;
