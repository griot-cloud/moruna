//! `moruna-image`: builds a boot image for the Moruna microkernel from a description
//! (SDD 23, f.8). The description names the processes the kernel starts, the program
//! each runs, the channels between them and the handles each starts with; the tool
//! checks every program is an image the kernel loads, renders the boot manifest with
//! `moruna_abi::manifest` (the code the kernel parses it with), and writes the
//! initramfs. Adding a server to an image is adding it to the description.
//!
//! ```text
//! moruna-image 1
//! process NAME [PROGRAM]            # PROGRAM defaults to NAME
//!   handle SOURCE SLOT [rights=R,...]
//!   driver VVVV:DDDD PROGRAM SERVICE  # a line of the process's driver table
//!   expect TEXT                     # a line the boot's console must carry
//!   exits CODE                      # the process ends with CODE
//!   faults TEXT                     # the process faults, as the kernel words it
//! link PROCESS.SLOT[:R,...] PROCESS.SLOT[:R,...]
//! expect TEXT                       # before any process: a line of the boot itself
//! ```
//!
//! `SOURCE` is `console`, `root.process`, `root.device`, `root.interrupt`, `root.ecam`,
//! `root.msi`, `root.dma`, or `program:PROGRAM`, a read-only memory object holding that
//! program's image, for a process that starts others. A process with `driver` lines
//! is a device manager (SDD 25, e.3): the tool renders its driver table
//! (`moruna_driver_table`) to `etc/NAME.drivers` and gives it as the slot `drivers`,
//! and gives each driver program's image as the slot `driver.PROGRAM`, so a new
//! driver is one line here. A `link` is a channel: the first process gets endpoint
//! 0 under its slot, the second endpoint 1. `expect`, `exits` and `faults` are what a
//! boot of the image must print; the tool writes them, with the kernel's own boot and
//! power-off lines, to the expectations file the boot check reads.

use std::{collections::BTreeSet, fmt, fs, path::Path};

use moruna_abi::{
    ABI_MAJOR, ABI_MINOR, IMAGE_RANGE, Rights,
    manifest::{HandleSpec, Manifest, ProcessSpec, Source},
    valid_name,
};
use moruna_driver_table::{Driver, Table};
use moruna_loader::{cpio, elf};

/// The description format version this tool reads.
pub const VERSION: &str = "1";
/// Where programs go inside the initramfs.
pub const BIN: &str = "bin";

/// A refusal, and the description line it is about (0 for the whole description).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescError {
    pub line: usize,
    pub reason: String,
}

impl fmt::Display for DescError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "line {}: {}", self.line, self.reason)
        }
    }
}

fn err<T>(line: usize, reason: impl Into<String>) -> Result<T, DescError> {
    Err(DescError {
        line,
        reason: reason.into(),
    })
}

/// Where a described handle comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescSource {
    Console,
    RootProcess,
    RootDevice,
    RootInterrupt,
    RootEcam,
    RootMsi,
    RootDma,
    Program(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescHandle {
    pub source: DescSource,
    pub slot: String,
    pub rights: Option<Rights>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescProcess {
    pub name: String,
    pub program: String,
    pub handles: Vec<DescHandle>,
    /// The process's driver table, if it manages devices.
    pub drivers: Table,
    pub expect: Vec<String>,
    pub line: usize,
}

impl DescProcess {
    /// Where the process's driver table goes in the initramfs.
    pub fn table_path(&self) -> String {
        format!("etc/{}.drivers", self.name)
    }
}

/// One end of a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub process: String,
    pub slot: String,
    pub rights: Option<Rights>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub ends: [End; 2],
    pub line: usize,
}

/// A parsed description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Description {
    pub processes: Vec<DescProcess>,
    pub links: Vec<Link>,
    /// Lines the boot itself must print, besides each process's.
    pub expect: Vec<String>,
}

fn rights_list(list: &str, line: usize) -> Result<Rights, DescError> {
    let mut rights = Rights::NONE;
    for name in list.split(',') {
        match Rights::from_name(name) {
            Some(r) => rights = rights | r,
            None => return err(line, format!("`{name}` is not a right")),
        }
    }
    Ok(rights)
}

fn name(text: &str, line: usize, what: &str) -> Result<String, DescError> {
    if valid_name(text.as_bytes()) {
        Ok(String::from(text))
    } else {
        err(
            line,
            format!("{what} `{text}` is not 1 to 32 bytes of [a-z0-9._-]"),
        )
    }
}

fn end(field: &str, line: usize) -> Result<End, DescError> {
    let (target, rights) = match field.split_once(':') {
        Some((t, r)) => (t, Some(rights_list(r, line)?)),
        None => (field, None),
    };
    let Some((process, slot)) = target.split_once('.') else {
        return err(line, format!("`{field}` is not PROCESS.SLOT"));
    };
    Ok(End {
        process: name(process, line, "process")?,
        slot: name(slot, line, "slot")?,
        rights,
    })
}

impl Description {
    /// Parses a description; every refusal names its line.
    pub fn parse(text: &str) -> Result<Description, DescError> {
        let mut d = Description::default();
        let mut header = false;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.trim();
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            let (directive, rest) = content
                .split_once(char::is_whitespace)
                .unwrap_or((content, ""));
            let rest = rest.trim();
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if !header {
                if directive != "moruna-image" || rest != VERSION {
                    return err(
                        line,
                        format!("the first line must be `moruna-image {VERSION}`"),
                    );
                }
                header = true;
                continue;
            }
            match directive {
                "process" => {
                    let (process, program) = match fields.as_slice() {
                        [p] => (*p, *p),
                        [p, program] => (*p, *program),
                        _ => return err(line, "`process NAME [PROGRAM]`"),
                    };
                    let process = name(process, line, "process")?;
                    if d.processes.iter().any(|p| p.name == process) {
                        return err(line, format!("process `{process}` is described twice"));
                    }
                    d.processes.push(DescProcess {
                        name: process,
                        program: name(program, line, "program")?,
                        handles: Vec::new(),
                        drivers: Table::default(),
                        expect: Vec::new(),
                        line,
                    });
                }
                "handle" => {
                    let (source, slot, rights) = match fields.as_slice() {
                        [s, slot] => (*s, *slot, None),
                        [s, slot, r] => match r.strip_prefix("rights=") {
                            Some(list) => (*s, *slot, Some(rights_list(list, line)?)),
                            None => {
                                return err(line, "the third field of `handle` is `rights=R,...`");
                            }
                        },
                        _ => return err(line, "`handle SOURCE SLOT [rights=R,...]`"),
                    };
                    let source = match source {
                        "console" => DescSource::Console,
                        "root.process" => DescSource::RootProcess,
                        "root.device" => DescSource::RootDevice,
                        "root.interrupt" => DescSource::RootInterrupt,
                        "root.ecam" => DescSource::RootEcam,
                        "root.msi" => DescSource::RootMsi,
                        "root.dma" => DescSource::RootDma,
                        other => match other.strip_prefix("program:") {
                            Some(p) => DescSource::Program(name(p, line, "program")?),
                            None => return err(line, format!("`{other}` is not a handle source")),
                        },
                    };
                    let slot = name(slot, line, "slot")?;
                    let Some(process) = d.processes.last_mut() else {
                        return err(line, "a `handle` line must follow a `process` line");
                    };
                    process.handles.push(DescHandle {
                        source,
                        slot,
                        rights,
                    });
                }
                "driver" => {
                    let [ids, program, service] = fields.as_slice() else {
                        return err(line, "`driver VVVV:DDDD PROGRAM SERVICE`");
                    };
                    let Some((vendor, device)) = moruna_driver_table::parse_ids(ids) else {
                        return err(line, format!("`{ids}` is not VVVV:DDDD in hex"));
                    };
                    if !moruna_driver_table::valid_program(program) {
                        return err(
                            line,
                            format!("program `{program}` is not 1 to 25 bytes of [a-z0-9._-]"),
                        );
                    }
                    if !moruna_driver_table::valid_service(service) {
                        return err(
                            line,
                            format!("service `{service}` is not 1 to 30 bytes of [a-z0-9._-]"),
                        );
                    }
                    let Some(process) = d.processes.last_mut() else {
                        return err(line, "a `driver` line must follow a `process` line");
                    };
                    process.drivers.drivers.push(Driver {
                        vendor,
                        device,
                        program: String::from(*program),
                        service: String::from(*service),
                    });
                    // The table's own rules (one driver a device, one line a service),
                    // checked here so the refusal names this line.
                    let rendered = process.drivers.to_string();
                    if let Err(e) = Table::parse(&rendered) {
                        return err(line, format!("the driver table: {}", e.reason));
                    }
                }
                "link" => {
                    let [a, b] = fields.as_slice() else {
                        return err(line, "`link PROCESS.SLOT[:R,...] PROCESS.SLOT[:R,...]`");
                    };
                    d.links.push(Link {
                        ends: [end(a, line)?, end(b, line)?],
                        line,
                    });
                }
                "expect" | "exits" | "faults" => {
                    if rest.is_empty() {
                        return err(line, format!("`{directive}` needs its text"));
                    }
                    let text = match directive {
                        "expect" => String::from(rest),
                        _ => {
                            let Some(process) = d.processes.last() else {
                                return err(
                                    line,
                                    format!("`{directive}` must follow a `process` line"),
                                );
                            };
                            if directive == "exits" {
                                let code: i64 = rest.parse().or_else(|_| {
                                    err(line, format!("`{rest}` is not an exit code"))
                                })?;
                                format!("kk: process {} exited with code {code} (", process.name)
                            } else {
                                format!("kk: process {} faulted: {rest}", process.name)
                            }
                        }
                    };
                    match d.processes.last_mut() {
                        Some(p) => p.expect.push(text),
                        None => d.expect.push(text),
                    }
                }
                other => return err(line, format!("`{other}` is not a directive")),
            }
        }
        if !header {
            return err(
                0,
                format!("the description is empty; it starts with `moruna-image {VERSION}`"),
            );
        }
        Ok(d)
    }

    /// Every program the image carries, by name.
    pub fn programs(&self) -> BTreeSet<String> {
        let mut programs = BTreeSet::new();
        for p in &self.processes {
            programs.insert(p.program.clone());
            for h in &p.handles {
                if let DescSource::Program(name) = &h.source {
                    programs.insert(name.clone());
                }
            }
            for d in &p.drivers.drivers {
                programs.insert(d.program.clone());
            }
        }
        programs
    }

    /// Every driver table the image carries: its path and its text.
    pub fn tables(&self) -> Vec<(String, String)> {
        self.processes
            .iter()
            .filter(|p| !p.drivers.drivers.is_empty())
            .map(|p| (p.table_path(), p.drivers.to_string()))
            .collect()
    }

    /// The boot manifest the description renders to, checked by parsing what it
    /// renders with the kernel's own parser.
    pub fn manifest(&self) -> Result<(Manifest, String), DescError> {
        let mut manifest = Manifest {
            channels: Vec::new(),
            processes: Vec::new(),
        };
        for p in &self.processes {
            let mut handles: Vec<HandleSpec> = p
                .handles
                .iter()
                .map(|h| HandleSpec {
                    source: match &h.source {
                        DescSource::Console => Source::Console,
                        DescSource::RootProcess => Source::RootProcess,
                        DescSource::RootDevice => Source::RootDevice,
                        DescSource::RootInterrupt => Source::RootInterrupt,
                        DescSource::RootEcam => Source::RootEcam,
                        DescSource::RootMsi => Source::RootMsi,
                        DescSource::RootDma => Source::RootDma,
                        DescSource::Program(program) => Source::File {
                            path: format!("{BIN}/{program}"),
                        },
                    },
                    slot: h.slot.clone(),
                    rights: h.rights,
                })
                .collect();
            if !p.drivers.drivers.is_empty() {
                handles.push(HandleSpec {
                    source: Source::File {
                        path: p.table_path(),
                    },
                    slot: String::from("drivers"),
                    rights: None,
                });
                for program in p.drivers.programs() {
                    handles.push(HandleSpec {
                        source: Source::File {
                            path: format!("{BIN}/{program}"),
                        },
                        slot: format!("{}{program}", moruna_driver_table::IMAGE_SLOT),
                        rights: None,
                    });
                }
            }
            manifest.processes.push(ProcessSpec {
                name: p.name.clone(),
                path: format!("{BIN}/{}", p.program),
                handles,
            });
        }
        for (index, link) in self.links.iter().enumerate() {
            let first = &link.ends[0];
            let readable = format!("{}.{}", first.process, first.slot);
            let channel =
                if valid_name(readable.as_bytes()) && !manifest.channels.contains(&readable) {
                    readable
                } else {
                    format!("link{index}")
                };
            manifest.channels.push(channel);
            for (side, end) in link.ends.iter().enumerate() {
                let Some(process) = manifest
                    .processes
                    .iter_mut()
                    .find(|p| p.name == end.process)
                else {
                    return err(
                        link.line,
                        format!("no process `{}` is described", end.process),
                    );
                };
                process.handles.push(HandleSpec {
                    source: Source::ChannelEnd {
                        channel: index,
                        end: side as u8,
                    },
                    slot: end.slot.clone(),
                    rights: end.rights,
                });
            }
        }
        let text = manifest.to_string();
        let parsed = Manifest::parse(text.as_bytes()).map_err(|e| {
            let at = text.lines().nth(e.line.saturating_sub(1)).unwrap_or("");
            DescError {
                line: 0,
                reason: format!(
                    "the manifest it renders is refused at `{at}`: {}",
                    e.reason.describe()
                ),
            }
        })?;
        if parsed != manifest {
            return err(
                0,
                "the manifest does not survive being rendered and parsed again",
            );
        }
        Ok((manifest, text))
    }

    /// Every line a boot of this image must print.
    pub fn expectations(&self) -> Vec<String> {
        let mut lines = self.expect.clone();
        lines.push(format!(
            "kk: moruna microkernel, abi {ABI_MAJOR}.{ABI_MINOR}: started {} processes from the manifest",
            self.processes.len()
        ));
        for p in &self.processes {
            lines.extend(p.expect.iter().cloned());
        }
        lines.push(String::from(
            "kk: every process has ended; the supervisor succeeded; powering off",
        ));
        lines
    }
}

/// Appends one newc entry.
fn entry(archive: &mut Vec<u8>, ino: u32, name: &str, mode: u32, data: &[u8]) {
    let fields = [
        ino,
        mode,
        0,
        0,
        1,
        0,
        data.len() as u32,
        0,
        0,
        0,
        0,
        name.len() as u32 + 1,
        0,
    ];
    archive.extend_from_slice(b"070701");
    for f in fields {
        archive.extend_from_slice(format!("{f:08x}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    archive.resize(archive.len().next_multiple_of(4), 0);
    archive.extend_from_slice(data);
    archive.resize(archive.len().next_multiple_of(4), 0);
}

/// A newc cpio archive of `files` (path, bytes) under their directories, in the order
/// given, read back with the kernel's own reader before it is returned.
pub fn archive(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, DescError> {
    let mut out = Vec::new();
    let mut dirs = BTreeSet::new();
    let mut ino = 1;
    for (path, data) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        parts.pop();
        for depth in 1..=parts.len() {
            let dir = parts[..depth].join("/");
            if dirs.insert(dir.clone()) {
                entry(&mut out, ino, &dir, 0o040755, &[]);
                ino += 1;
            }
        }
        entry(&mut out, ino, path, 0o100644, data);
        ino += 1;
    }
    entry(&mut out, 0, "TRAILER!!!", 0, &[]);
    let read = cpio::files(&out).map_err(|e| DescError {
        line: 0,
        reason: format!("the archive written does not read back: {}", e.describe()),
    })?;
    if read.len() != files.len()
        || read
            .iter()
            .zip(files)
            .any(|(r, (p, d))| r.path != p || r.data != &d[..])
    {
        return err(0, "the archive written does not read back as written");
    }
    Ok(out)
}

/// What a build produces.
pub struct Image {
    pub manifest: String,
    pub initramfs: Vec<u8>,
    pub expect: Vec<String>,
}

/// Builds the image a description names, its programs read from `programs`.
pub fn build(description: &str, programs: &Path) -> Result<Image, DescError> {
    let d = Description::parse(description)?;
    let (_, manifest) = d.manifest()?;
    let mut files = Vec::new();
    for program in d.programs() {
        let path = programs.join(&program);
        let bytes = fs::read(&path)
            .or_else(|e| err(0, format!("program `{program}`: {}: {e}", path.display())))?;
        elf::parse(&bytes, IMAGE_RANGE).map_err(|e| DescError {
            line: 0,
            reason: format!(
                "program `{program}` is not an image the kernel loads: {}",
                e.describe()
            ),
        })?;
        files.push((format!("{BIN}/{program}"), bytes));
    }
    files.extend(
        d.tables()
            .into_iter()
            .map(|(path, text)| (path, text.into_bytes())),
    );
    files.push((
        String::from(moruna_abi::manifest::FILE_NAME),
        manifest.clone().into_bytes(),
    ));
    Ok(Image {
        initramfs: archive(&files)?,
        manifest,
        expect: d.expectations(),
    })
}
