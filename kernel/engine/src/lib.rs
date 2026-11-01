//! The engine's start, as the boot manifest describes it (SDD 30 d.3): the configuration
//! the supervisor reads from its `config` data handle, so no path, argument, budget or
//! service is written into a program.
//!
//! ```text
//! # comments and blank lines are skipped
//! program /moruna/bin/moruna          # the engine, a path in the Linux image
//! arg serve                           # each argument after argv[0], in order
//! env HOME=/tmp                       # each environment entry, in order
//! memory 1073741824                   # the budget its machine has, in bytes
//! scratch 268435456                   # its scratch directory's quota, in bytes
//! grant vsock lookup vsock0           # slot `vsock`: a connection to a directory service
//! grant linux personality             # slot `linux`: the personality's listener, to start tasks
//! grant network service network      # fresh connection from explicitly granted listener
//! grant image data image               # slot `image`: a read-only duplicate of the supervisor's
//!                                      # own data handle `image`, the attached image's record
//! runs 3                              # how many times it is started, one after another
//! ```
//!
//! Every line is checked; an unknown key, a repeated one that may appear once, a number
//! that is not one, a slot name the kernel would refuse and a configuration with no
//! program or no budget are refused with the line.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

/// Where a granted handle comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A new connection to a service the device manager's directory names.
    Lookup(String),
    /// A duplicate of the Linux personality's listener: the engine connects to it for
    /// each task it starts.
    Personality,
    /// A read-only duplicate of a data handle the supervisor itself starts with, under
    /// this slot name: a file of the boot's (SDD 30 f.12, the attached image's record).
    Data(String),
    /// A fresh connection, for each run, to an explicitly granted listener on the
    /// supervisor's start page. Never a directory lookup or a read-only memory grant.
    Service(String),
    /// Already-connected private storage authority and canonical Admission data.
    Admission { authority: String, control: String },
}

/// One handle the engine starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// Its slot name on the engine's start page.
    pub slot: String,
    pub source: Source,
}

/// The engine's start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub program: String,
    /// `argv[1..]`; `argv[0]` is the program.
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub memory: u64,
    pub scratch: u64,
    pub grants: Vec<Grant>,
    pub runs: u32,
}

/// The most runs one boot asks for.
pub const RUNS_MAX: u32 = 64;

fn slot_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn source_roles(source: &Source) -> Vec<(&str, u8)> {
    match source {
        Source::Data(name) => alloc::vec![(name.as_str(), 0)],
        Source::Service(name) => alloc::vec![(name.as_str(), 1)],
        Source::Admission { authority, control } => {
            alloc::vec![(authority.as_str(), 2), (control.as_str(), 0)]
        }
        _ => Vec::new(),
    }
}

impl Config {
    /// Parses a configuration; a refusal names its line (1-based) and why.
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut program = None;
        let mut args = Vec::new();
        let mut env = Vec::new();
        let mut memory = None;
        let mut scratch = None;
        let mut grants: Vec<Grant> = Vec::new();
        let mut runs = None;
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            let rest = rest.trim();
            let refuse = |why: &str| format!("line {n}: {why}: `{raw}`");
            let once = |seen: bool| {
                if seen {
                    Err(refuse("given twice"))
                } else {
                    Ok(())
                }
            };
            let number = |s: &str| s.parse::<u64>().map_err(|_| refuse("not a number"));
            match key {
                "program" => {
                    once(program.is_some())?;
                    if !rest.starts_with('/') || rest.contains(' ') {
                        return Err(refuse("the program is one absolute path"));
                    }
                    program = Some(String::from(rest));
                }
                "arg" => args.push(String::from(rest)),
                "env" => {
                    if !rest.contains('=') {
                        return Err(refuse("an environment entry is NAME=value"));
                    }
                    env.push(String::from(rest));
                }
                "memory" => {
                    once(memory.is_some())?;
                    let m = number(rest)?;
                    if m == 0 {
                        return Err(refuse("a budget of no memory"));
                    }
                    if m % 4096 != 0 {
                        return Err(refuse("memory must be whole pages"));
                    }
                    memory = Some(m);
                }
                "scratch" => {
                    once(scratch.is_some())?;
                    let bytes = number(rest)?;
                    if bytes == 0 || bytes % 4096 != 0 {
                        return Err(refuse("scratch must be positive whole pages"));
                    }
                    scratch = Some(bytes);
                }
                "runs" => {
                    once(runs.is_some())?;
                    let r = number(rest)?;
                    if r == 0 || r > RUNS_MAX as u64 {
                        return Err(refuse("runs is 1 to 64"));
                    }
                    runs = Some(r as u32);
                }
                "grant" => {
                    let mut words = rest.split_whitespace();
                    let slot = words.next().unwrap_or("");
                    if slot == "account" {
                        return Err(refuse("the runtime account slot is reserved"));
                    }
                    if !slot_ok(slot) {
                        return Err(refuse("a slot name is 1 to 32 of [a-z0-9._-]"));
                    }
                    if grants.iter().any(|g| g.slot == slot) {
                        return Err(refuse("the slot is granted twice"));
                    }
                    let kind = words.next();
                    let first = words.next();
                    let second = words.next();
                    let source = match (kind, first, second) {
                        (Some("admission"), Some(authority), Some(control))
                            if slot_ok(authority)
                                && slot_ok(control)
                                && authority != control
                                && authority != "account"
                                && control != "account"
                                && words.next().is_none() =>
                        {
                            Source::Admission {
                                authority: authority.into(),
                                control: control.into(),
                            }
                        }
                        (Some("lookup"), Some(service), None) => {
                            Source::Lookup(String::from(service))
                        }
                        (Some("personality"), None, None) => Source::Personality,
                        (Some("data"), Some(from), None) if slot_ok(from) && from != "account" => {
                            Source::Data(String::from(from))
                        }
                        (Some("service"), Some(from), None)
                            if slot_ok(from) && from != "account" =>
                        {
                            Source::Service(String::from(from))
                        }
                        _ => {
                            return Err(refuse(
                                "a grant is `grant SLOT lookup SERVICE`, `grant SLOT personality` \
                                 or `grant SLOT data SLOT` or `grant SLOT service SLOT`",
                            ));
                        }
                    };
                    // A start-page handle has one role, so a config cannot claim the same
                    // handle is both data memory and a service listener.
                    let mixed_role = grants.iter().any(|grant| {
                        source_roles(&grant.source).iter().any(|(a, role)| {
                            source_roles(&source)
                                .iter()
                                .any(|(b, other)| a == b && role != other)
                        })
                    });
                    if mixed_role {
                        return Err(refuse(
                            "a start slot cannot have different data/listener/admission roles",
                        ));
                    }
                    grants.push(Grant {
                        slot: String::from(slot),
                        source,
                    });
                }
                _ => return Err(refuse("not a key this configuration has")),
            }
        }
        Ok(Config {
            program: program.ok_or_else(|| String::from("no `program` line"))?,
            args,
            env,
            memory: memory.ok_or_else(|| String::from("no `memory` line: the engine's budget"))?,
            scratch: scratch.ok_or_else(|| {
                String::from("no `scratch` line: an explicit scratch account is required")
            })?,
            grants,
            runs: runs.unwrap_or(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "# the engine\n\
        program /moruna/bin/moruna\n\
        arg serve\n\
        arg --listen\n\
        arg vsock://-1:5100   # the embedder connects here\n\
        env HOME=/tmp\n\
        memory 1073741824\n\
        scratch 268435456\n\
        \n\
        grant vsock lookup vsock0\n\
        grant linux personality\n\
        grant image data record\n\
        runs 3\n";

    #[test]
    fn private_admission_is_distinct_from_data_and_public_listeners() {
        let base = "program /e\nmemory 4096\nscratch 4096\n";
        let config = Config::parse(&format!(
            "{base}grant storage admission authority binding\n"
        ))
        .unwrap();
        assert_eq!(
            config.grants[0].source,
            Source::Admission {
                authority: "authority".into(),
                control: "binding".into()
            }
        );
        for extra in [
            "grant storage admission same same\n",
            "grant storage admission authority binding extra\n",
            "grant storage admission account binding\n",
            "grant storage admission authority account\n",
            "grant storage admission authority binding\ngrant other service authority\n",
            "grant storage admission authority binding\ngrant other data authority\n",
            "grant storage admission authority binding\ngrant other service binding\n",
        ] {
            assert!(
                Config::parse(&format!("{base}{extra}")).is_err(),
                "accepted {extra}"
            );
        }
    }

    #[test]
    fn kd2_t5_a_configuration_reads_whole() {
        let c = Config::parse(GOOD).unwrap();
        assert_eq!(c.program, "/moruna/bin/moruna");
        assert_eq!(c.args, ["serve", "--listen", "vsock://-1:5100"]);
        assert_eq!(c.env, ["HOME=/tmp"]);
        assert_eq!((c.memory, c.scratch, c.runs), (1 << 30, 256 << 20, 3));
        assert_eq!(
            c.grants,
            [
                Grant {
                    slot: "vsock".into(),
                    source: Source::Lookup("vsock0".into())
                },
                Grant {
                    slot: "linux".into(),
                    source: Source::Personality
                },
                Grant {
                    slot: "image".into(),
                    source: Source::Data("record".into())
                }
            ]
        );
        let least = Config::parse("program /e\nmemory 4096\nscratch 4096\n").unwrap();
        assert_eq!(
            (least.runs, least.scratch, least.grants.len()),
            (1, 4096, 0)
        );
    }

    #[test]
    fn kd2_t5_service_grants_are_explicit_and_distinct_from_data_or_lookup() {
        let config = Config::parse(
            "program /e\nmemory 4096\nscratch 4096\ngrant network service net-listener\nruns 2\n",
        )
        .unwrap();
        assert_eq!(
            config.grants,
            [Grant {
                slot: "network".into(),
                source: Source::Service("net-listener".into())
            }]
        );
        assert_eq!(config.runs, 2);
        for suffix in ["service", "service Network", "service network extra"] {
            let error = Config::parse(&format!(
                "program /e\nmemory 4096\ngrant network {suffix}\n"
            ))
            .unwrap_err();
            assert!(error.starts_with("line 3: a grant is"), "{error}");
        }
        for grants in [
            "grant a data network\ngrant b service network",
            "grant a service network\ngrant b data network",
        ] {
            let error = Config::parse(&format!("program /e\nmemory 4096\n{grants}\n")).unwrap_err();
            assert!(
                error.starts_with(
                    "line 4: a start slot cannot have different data/listener/admission roles"
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn kd2_account_contract_refuses_shadow_and_implicit_scratch() {
        for source in [
            "data record",
            "personality",
            "service storage",
            "lookup vsock0",
        ] {
            let error = Config::parse(&format!(
                "program /e\nmemory 4096\nscratch 4096\ngrant account {source}\n"
            ))
            .unwrap_err();
            assert!(
                error.contains("runtime account slot is reserved"),
                "{error}"
            );
        }
        assert!(
            Config::parse("program /e\nmemory 4096\n")
                .unwrap_err()
                .contains("scratch")
        );
        assert!(
            Config::parse("program /e\nmemory 4096\nscratch 0\n")
                .unwrap_err()
                .contains("positive whole pages")
        );
        assert!(
            Config::parse("program /e\nmemory 1\nscratch 4096\n")
                .unwrap_err()
                .contains("whole pages")
        );
    }

    #[test]
    fn kd2_t5_every_refusal_names_its_line() {
        let cases = [
            ("memory 4096\n", "no `program` line"),
            ("program /e\n", "no `memory` line"),
            (
                "program e\nmemory 4096\n",
                "line 1: the program is one absolute path",
            ),
            (
                "program /e\nprogram /f\nmemory 4096\n",
                "line 2: given twice",
            ),
            ("program /e\nmemory lots\n", "line 2: not a number"),
            ("program /e\nmemory 0\n", "line 2: a budget of no memory"),
            (
                "program /e\nmemory 4096\nruns 0\n",
                "line 3: runs is 1 to 64",
            ),
            (
                "program /e\nmemory 4096\nruns 65\n",
                "line 3: runs is 1 to 64",
            ),
            (
                "program /e\nmemory 4096\nenv HOME\n",
                "line 3: an environment entry",
            ),
            (
                "program /e\nmemory 4096\ngrant Vsock personality\n",
                "line 3: a slot name",
            ),
            (
                "program /e\nmemory 4096\ngrant v lookup\n",
                "line 3: a grant is",
            ),
            (
                "program /e\nmemory 4096\ngrant v process\n",
                "line 3: a grant is",
            ),
            (
                "program /e\nmemory 4096\ngrant v personality\ngrant v lookup x\n",
                "line 4: the slot is granted twice",
            ),
            ("program /e\nmemory 4096\nbudget 3\n", "line 3: not a key"),
            (
                "program /e\nmemory 4096\ngrant i data\n",
                "line 3: a grant is",
            ),
            (
                "program /e\nmemory 4096\ngrant i data Record\n",
                "line 3: a grant is",
            ),
        ];
        for (text, want) in cases {
            let got = Config::parse(text).unwrap_err();
            assert!(
                got.starts_with(want) || got.contains(want),
                "{text:?}: {got}"
            );
        }
    }
}
