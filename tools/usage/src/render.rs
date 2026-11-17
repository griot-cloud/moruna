//! The report as Markdown, for people; `usage.json` holds the same for programs.

use std::{collections::BTreeMap, fmt::Write as _};

use crate::report::{GROUPS, Report, Scope};

fn cell(count: Option<u64>) -> String {
    count.map_or_else(|| "-".to_string(), |c| c.to_string())
}

fn percent(covered: u64, total: u64) -> String {
    if total == 0 {
        return "-".into();
    }
    format!(
        "{covered} / {total} ({:.1}%)",
        covered as f64 * 100.0 / total as f64
    )
}

/// A table of a dispatcher's arms: one column per job and one for all.
fn dispatch_table(out: &mut String, what: &str, jobs: &[Scope], all: &Scope) {
    let _ = write!(out, "| call | number |");
    for j in jobs {
        let _ = write!(out, " {} |", j.label);
    }
    let _ = writeln!(out, " all |");
    let _ = writeln!(out, "|{}", " --- |".repeat(jobs.len() + 3));
    let Some(arms) = all.dispatch.get(what) else {
        return;
    };
    for (i, arm) in arms.iter().enumerate() {
        // Native numbers in hex (from 0x10000), Linux numbers as Linux writes them.
        let numbers: Vec<String> = arm
            .numbers
            .iter()
            .map(|n| {
                if *n >= 0x1_0000 {
                    format!("{n:#x}")
                } else {
                    n.to_string()
                }
            })
            .collect();
        let _ = write!(
            out,
            "| {} | {} |",
            arm.names.join(" \\| "),
            numbers.join(", ")
        );
        for j in jobs {
            let _ = write!(
                out,
                " {} |",
                cell(
                    j.dispatch
                        .get(what)
                        .and_then(|a| a.get(i))
                        .and_then(|a| a.count)
                )
            );
        }
        let _ = writeln!(out, " {} |", cell(arm.count));
    }
}

/// The Markdown report. `names` maps Linux call numbers to names, for the refused table.
pub fn markdown(report: &Report, names: &BTreeMap<u64, String>) -> String {
    let mut out = String::new();
    let jobs = &report.jobs;
    let all = &report.all;
    let _ = writeln!(out, "# What the gate's jobs used\n");
    let _ = writeln!(
        out,
        "The usage build (stream U): each job run once, untimed, with coverage counters in kernel \
         mode and in every program of the node. Counts are exact (atomic counters).\n"
    );
    for j in jobs {
        let _ = writeln!(
            out,
            "- **{}**: {} program dumps, {} cut short or refused",
            j.label, j.dumps, j.broken_dumps
        );
    }
    for note in &report.notes {
        let _ = writeln!(out, "- {note}");
    }

    let _ = writeln!(out, "\n## Native calls the kernel answered\n");
    dispatch_table(&mut out, "native", jobs, all);
    let _ = writeln!(
        out,
        "\n## Native calls each program made (all jobs, from moruna-sys)\n"
    );
    let _ = writeln!(out, "| program | calls |\n| --- | --- |");
    for (name, p) in &all.programs {
        let calls: Vec<String> = p.calls.iter().map(|(c, n)| format!("{c} {n}")).collect();
        let _ = writeln!(out, "| {name} | {} |", calls.join(", "));
    }

    let _ = writeln!(
        out,
        "\n## Linux calls\n\n### Answered by the Linux server\n"
    );
    dispatch_table(&mut out, "linux-server", jobs, all);
    let _ = writeln!(out, "\n### Answered in kernel mode (the fast path)\n");
    dispatch_table(&mut out, "linux-kernel", jobs, all);
    let _ = writeln!(out, "\n### Refused\n");
    let _ = write!(out, "| call | number |");
    for j in jobs {
        let _ = write!(out, " {} |", j.label);
    }
    let _ = writeln!(out, " all |\n|{}", " --- |".repeat(jobs.len() + 3));
    for (n, c) in &all.refused {
        let _ = write!(
            out,
            "| {} | {n} |",
            names.get(n).map_or("?", String::as_str)
        );
        for j in jobs {
            let _ = write!(out, " {} |", j.refused.get(n).copied().unwrap_or(0));
        }
        let _ = writeln!(out, " {c} |");
    }
    let _ = writeln!(
        out,
        "\nLinux processes that ended, with the calls each was served:\n"
    );
    for j in jobs {
        let ended: Vec<String> = j
            .linux_processes
            .iter()
            .map(|(n, s)| format!("{n} ({s})"))
            .collect();
        let _ = writeln!(
            out,
            "- {}: {}",
            j.label,
            if ended.is_empty() {
                "none".into()
            } else {
                ended.join(", ")
            }
        );
    }

    let _ = writeln!(out, "\n## Programs the boot started\n");
    let _ = write!(
        out,
        "| program | processes | events handled | lines run (all jobs) |"
    );
    for j in jobs {
        let _ = write!(out, " {} |", j.label);
    }
    let _ = writeln!(out, "\n|{}", " --- |".repeat(jobs.len() + 4));
    for (name, p) in &all.programs {
        let processes: Vec<String> = p
            .processes
            .iter()
            .map(|(n, c)| format!("{n} x{c}"))
            .collect();
        let _ = write!(
            out,
            "| {name} | {} | {} | {} |",
            processes.join(", "),
            p.wakes,
            percent(p.lines.0, p.lines.1)
        );
        for j in jobs {
            let _ = write!(
                out,
                " {} |",
                j.programs
                    .get(name)
                    .map_or("not started".to_string(), |u| format!("{} events", u.wakes))
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(
        out,
        "\n## Kernel-mode code run\n\nLines run of the lines mapped (which include generic code \
         never instantiated), per job and over all jobs; then functions run of those compiled \
         into the image.\n"
    );
    let _ = write!(out, "| area |");
    for j in jobs {
        let _ = write!(out, " {} |", j.label);
    }
    let _ = writeln!(
        out,
        " all | functions run (all) |\n|{}",
        " --- |".repeat(jobs.len() + 3)
    );
    for (area, a) in &all.kernel_areas {
        let _ = write!(out, "| {area} |");
        for j in jobs {
            let b = j.kernel_areas.get(area).copied().unwrap_or_default();
            let _ = write!(out, " {} |", percent(b[0], b[1]));
        }
        let _ = writeln!(out, " {} | {} |", percent(a[0], a[1]), percent(a[2], a[3]));
    }

    let _ = writeln!(
        out,
        "\n## Candidates for removal\n\nNothing below was used by any job. The groups go from what \
         user space can reach most directly to what only the kernel reaches, and within a group \
         from the largest. Removal waits for the large datasets (FineWeb, Overture).\n"
    );
    for (letter, title) in GROUPS {
        let items: Vec<_> = report.unused.iter().filter(|i| i.group == letter).collect();
        let lines: u64 = items.iter().map(|i| i.lines).sum();
        let bytes: u64 = items.iter().map(|i| i.bytes).sum();
        let _ = writeln!(
            out,
            "### {letter}. {title}: {} items, {lines} lines, {bytes} bytes\n",
            items.len()
        );
        if items.is_empty() {
            let _ = writeln!(out, "None.\n");
            continue;
        }
        let _ = writeln!(
            out,
            "| item | lines | bytes | detail |\n| --- | --- | --- | --- |"
        );
        for i in items {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                i.name, i.lines, i.bytes, i.detail
            );
        }
        let _ = writeln!(out);
    }
    let lines: u64 = report.keep.iter().map(|i| i.lines).sum();
    let bytes: u64 = report.keep.iter().map(|i| i.bytes).sum();
    let _ = writeln!(
        out,
        "### Keep: accelerator path ({lines} lines, {bytes} bytes)\n\nKept whatever their use, for \
         accelerators behind the same task interface (Brackly, 2026-11-17): the device and DMA \
         calls, the drivers and the device manager, and the kernel-mode code they need.\n"
    );
    let _ = writeln!(
        out,
        "| part | kind | lines | bytes | use |\n| --- | --- | --- | --- | --- |"
    );
    for i in &report.keep {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            i.name, i.group, i.lines, i.bytes, i.detail
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{CallUse, Item, ProgramUse};

    #[test]
    fn the_report_has_its_tables_and_groups_in_order() {
        let arm = |count| CallUse {
            names: vec!["CHANNEL_READ".into()],
            numbers: vec![0x10006],
            line: 7,
            count,
        };
        let mut job = Scope {
            label: "nyc-score".into(),
            dumps: 12,
            ..Scope::default()
        };
        job.dispatch.insert("native".into(), vec![arm(Some(5))]);
        job.refused.insert(334, 1);
        job.linux_processes.push(("moruna-engine".into(), 40));
        job.programs.insert(
            "linux".into(),
            ProgramUse {
                wakes: 3,
                ..ProgramUse::default()
            },
        );
        job.kernel_areas.insert("OSTD 0.19".into(), [10, 40, 3, 6]);
        let mut all = job.clone();
        all.label = "all jobs".into();
        all.dispatch.insert("native".into(), vec![arm(None)]);
        all.programs.insert(
            "linux".into(),
            ProgramUse {
                processes: BTreeMap::from([("linux".into(), 1)]),
                wakes: 3,
                lines: (5, 10),
                calls: BTreeMap::from([("WAIT".into(), 3)]),
            },
        );
        let item = |group: &str, name: &str| Item {
            group: group.into(),
            name: name.into(),
            detail: "d".into(),
            lines: 2,
            bytes: 64,
        };
        let report = Report {
            jobs: vec![job],
            all,
            unused: vec![item("A", "SLOT_REFUSE"), item("E", "ostd/src/boot")],
            keep: vec![item("native call", "MEMORY_PIN")],
            notes: vec!["a note".into()],
        };
        let md = markdown(&report, &BTreeMap::from([(334, "rseq".to_string())]));
        for expect in [
            "- **nyc-score**: 12 program dumps",
            "- a note",
            "| CHANNEL_READ | 0x10006 | 5 | - |",
            "| linux | WAIT 3 |",
            "| rseq | 334 | 1 | 1 |",
            "- nyc-score: moruna-engine (40)",
            "| linux | linux x1 | 3 | 5 / 10 (50.0%) | 3 events |",
            "| OSTD 0.19 | 10 / 40 (25.0%) | 10 / 40 (25.0%) | 3 / 6 (50.0%) |",
            "### A. Native calls never made (any process can make them): 1 items, 2 lines, 64 bytes",
            "### B. Kernel-mode code reachable from user space, never run: 0 items",
            "| SLOT_REFUSE | 2 | 64 | d |",
            "### Keep: accelerator path (2 lines, 64 bytes)",
            "| MEMORY_PIN | native call | 2 | 64 | d |",
        ] {
            assert!(md.contains(expect), "missing {expect:?} in\n{md}");
        }
        let a = md.find("### A.").unwrap();
        let e = md.find("### E.").unwrap();
        let keep = md.find("### Keep").unwrap();
        assert!(a < e && e < keep);
        assert_eq!(percent(0, 0), "-");
    }
}
