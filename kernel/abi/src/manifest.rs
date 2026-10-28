//! The boot manifest (SDD 20, e.4): what the kernel starts, and the handles each
//! process starts with. It is the extension point: adding a server is adding a
//! `process` line, its `handle` lines and its ELF to the initramfs.
//!
//! ```text
//! # comment
//! moruna-manifest 1
//! channel NAME
//! process NAME PATH
//! handle SOURCE SLOT [rights=R,R,...]
//! ```
//!
//! `SOURCE` is `console`, `root.process`, `root.device`, `root.interrupt`,
//! `root.ecam`, `root.msi`, `root.dma`, `channel:NAME.0`, `channel:NAME.1` or
//! `file:PATH`. A `handle` line gives a handle to
//! the process named by the `process` line above it. The first process is the
//! supervisor: it alone may take a `root.*` source. Every endpoint of every declared
//! channel is given exactly once. Names are 1 to 32 bytes of `[a-z0-9._-]`; paths are
//! 1 to 128 bytes of `[A-Za-z0-9._/-]` and never start with `/`.

use alloc::{string::String, vec::Vec};
use core::fmt;

use crate::{Rights, valid_name};

/// The format version this parser reads.
pub const VERSION: u32 = 1;
/// The largest manifest, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;
/// The longest line, in bytes.
pub const MAX_LINE: usize = 512;
/// The most processes a manifest starts.
pub const MAX_PROCESSES: usize = 32;
/// The most channels a manifest declares.
pub const MAX_CHANNELS: usize = 64;
/// The longest path, in bytes.
pub const MAX_PATH: usize = 128;
/// The name of the manifest inside the initramfs.
pub const FILE_NAME: &str = "manifest";

/// Where a start handle comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The kernel's debug console, write only.
    Console,
    /// The root resource for `process_create`.
    RootProcess,
    /// The root resource for `device_memory_create`.
    RootDevice,
    /// The root resource for `interrupt_create`.
    RootInterrupt,
    /// A device memory resource over exactly the PCI configuration space the firmware
    /// describes (ACPI MCFG), so the supervisor can find and enumerate it (ABI 1.2).
    RootEcam,
    /// The root resource for `interrupt_create_msi` (ABI 1.2).
    RootMsi,
    /// The root resource for `memory_pin` (ABI 1.2).
    RootDma,
    /// One endpoint (0 or 1) of the channel declared at `channel` (an index into
    /// [`Manifest::channels`]).
    ChannelEnd { channel: usize, end: u8 },
    /// A read-only memory object holding the initramfs file at `path`.
    File { path: String },
}

impl Source {
    /// Whether only the supervisor may take this source.
    pub fn is_root(&self) -> bool {
        matches!(
            self,
            Source::RootProcess
                | Source::RootDevice
                | Source::RootInterrupt
                | Source::RootEcam
                | Source::RootMsi
                | Source::RootDma
        )
    }
}

/// One `handle` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleSpec {
    pub source: Source,
    /// The name the process looks the handle up by in its start page.
    pub slot: String,
    /// The rights to give, a subset of the source's; `None` gives all of the source's.
    pub rights: Option<Rights>,
}

/// One `process` line and the `handle` lines under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub name: String,
    /// The ELF inside the initramfs.
    pub path: String,
    pub handles: Vec<HandleSpec>,
}

/// A parsed, validated manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub channels: Vec<String>,
    /// In start order; the first is the supervisor.
    pub processes: Vec<ProcessSpec>,
}

/// Why a manifest was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    TooLarge,
    LineTooLong,
    NotText,
    MissingHeader,
    BadVersion,
    UnknownDirective,
    WrongArity,
    BadName,
    BadPath,
    DuplicateChannel,
    DuplicateProcess,
    DuplicateSlot,
    HandleBeforeProcess,
    UnknownSource,
    UnknownChannel,
    EndpointReused,
    EndpointUnused,
    RootOutsideSupervisor,
    BadRights,
    TooManyProcesses,
    TooManyChannels,
    TooManyHandles,
    NoProcesses,
}

impl Reason {
    pub fn describe(&self) -> &'static str {
        match self {
            Reason::TooLarge => "the manifest is larger than 64 KiB",
            Reason::LineTooLong => "a line is longer than 512 bytes",
            Reason::NotText => "the manifest is not ASCII text",
            Reason::MissingHeader => "the first directive is not `moruna-manifest`",
            Reason::BadVersion => "the manifest version is not 1",
            Reason::UnknownDirective => "unknown directive",
            Reason::WrongArity => "wrong number of fields",
            Reason::BadName => "a name is not 1 to 32 bytes of [a-z0-9._-]",
            Reason::BadPath => "a path is not 1 to 128 bytes of [A-Za-z0-9._/-], relative",
            Reason::DuplicateChannel => "a channel is declared twice",
            Reason::DuplicateProcess => "a process name is used twice",
            Reason::DuplicateSlot => "a slot name is used twice in one process",
            Reason::HandleBeforeProcess => "a `handle` line comes before any `process` line",
            Reason::UnknownSource => "unknown handle source",
            Reason::UnknownChannel => "a channel endpoint names an undeclared channel",
            Reason::EndpointReused => "a channel endpoint is given twice",
            Reason::EndpointUnused => "a channel endpoint is never given",
            Reason::RootOutsideSupervisor => {
                "a root resource is given to a process other than the first"
            }
            Reason::BadRights => "unknown right, or rights the source does not carry",
            Reason::TooManyProcesses => "more than 32 processes",
            Reason::TooManyChannels => "more than 64 channels",
            Reason::TooManyHandles => "more than 64 handles for one process",
            Reason::NoProcesses => "the manifest starts no process",
        }
    }
}

/// A refusal and the line it is about (1-based; 0 for the whole file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestError {
    pub line: usize,
    pub reason: Reason,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "manifest line {}: {}", self.line, self.reason.describe())
    }
}

fn err(line: usize, reason: Reason) -> ManifestError {
    ManifestError { line, reason }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && !path.split('/').any(|part| part.is_empty() || part == "..")
}

fn parse_rights(field: &str, line: usize) -> Result<Rights, ManifestError> {
    let list = field
        .strip_prefix("rights=")
        .ok_or(err(line, Reason::BadRights))?;
    let mut rights = Rights::NONE;
    for name in list.split(',') {
        rights = rights | Rights::from_name(name).ok_or(err(line, Reason::BadRights))?;
    }
    Ok(rights)
}

/// The rights a source carries before any `rights=` narrows them.
pub fn source_rights(source: &Source) -> Rights {
    match source {
        Source::Console => Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
        Source::RootProcess
        | Source::RootDevice
        | Source::RootInterrupt
        | Source::RootEcam
        | Source::RootMsi
        | Source::RootDma => Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
        Source::ChannelEnd { .. } => {
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER | Rights::WAIT
        }
        Source::File { .. } => Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER,
    }
}

impl Manifest {
    /// Parses and validates a manifest. Every refusal names its line.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        if bytes.len() > MAX_BYTES {
            return Err(err(0, Reason::TooLarge));
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii() && (*b >= 0x20 || matches!(b, b'\n' | b'\t' | b'\r')))
        {
            return Err(err(0, Reason::NotText));
        }
        let text = core::str::from_utf8(bytes).map_err(|_| err(0, Reason::NotText))?;

        let mut manifest = Manifest {
            channels: Vec::new(),
            processes: Vec::new(),
        };
        let mut header = false;
        // Which endpoints have been given, per channel.
        let mut given: Vec<[bool; 2]> = Vec::new();

        for (index, raw) in text.split('\n').enumerate() {
            let line = index + 1;
            if raw.len() > MAX_LINE {
                return Err(err(line, Reason::LineTooLong));
            }
            let content = raw.split('#').next().unwrap_or("");
            let fields: Vec<&str> = content.split_ascii_whitespace().collect();
            let Some(&directive) = fields.first() else {
                continue;
            };
            if !header {
                if directive != "moruna-manifest" {
                    return Err(err(line, Reason::MissingHeader));
                }
                if fields.len() != 2 {
                    return Err(err(line, Reason::WrongArity));
                }
                if fields[1] != "1" {
                    return Err(err(line, Reason::BadVersion));
                }
                header = true;
                continue;
            }
            match directive {
                "channel" => {
                    if fields.len() != 2 {
                        return Err(err(line, Reason::WrongArity));
                    }
                    let name = fields[1];
                    if !valid_name(name.as_bytes()) {
                        return Err(err(line, Reason::BadName));
                    }
                    if manifest.channels.iter().any(|c| c == name) {
                        return Err(err(line, Reason::DuplicateChannel));
                    }
                    if manifest.channels.len() == MAX_CHANNELS {
                        return Err(err(line, Reason::TooManyChannels));
                    }
                    manifest.channels.push(String::from(name));
                    given.push([false; 2]);
                }
                "process" => {
                    if fields.len() != 3 {
                        return Err(err(line, Reason::WrongArity));
                    }
                    let (name, path) = (fields[1], fields[2]);
                    if !valid_name(name.as_bytes()) {
                        return Err(err(line, Reason::BadName));
                    }
                    if !valid_path(path) {
                        return Err(err(line, Reason::BadPath));
                    }
                    if manifest.processes.iter().any(|p| p.name == name) {
                        return Err(err(line, Reason::DuplicateProcess));
                    }
                    if manifest.processes.len() == MAX_PROCESSES {
                        return Err(err(line, Reason::TooManyProcesses));
                    }
                    manifest.processes.push(ProcessSpec {
                        name: String::from(name),
                        path: String::from(path),
                        handles: Vec::new(),
                    });
                }
                "handle" => {
                    if fields.len() != 3 && fields.len() != 4 {
                        return Err(err(line, Reason::WrongArity));
                    }
                    let supervisor = manifest.processes.len() == 1;
                    let source = match fields[1] {
                        "console" => Source::Console,
                        "root.process" => Source::RootProcess,
                        "root.device" => Source::RootDevice,
                        "root.interrupt" => Source::RootInterrupt,
                        "root.ecam" => Source::RootEcam,
                        "root.msi" => Source::RootMsi,
                        "root.dma" => Source::RootDma,
                        other => {
                            if let Some(end) = other.strip_prefix("channel:") {
                                let (name, which) = match end.rsplit_once('.') {
                                    Some((n, "0")) => (n, 0u8),
                                    Some((n, "1")) => (n, 1u8),
                                    _ => return Err(err(line, Reason::UnknownSource)),
                                };
                                let channel = manifest
                                    .channels
                                    .iter()
                                    .position(|c| c == name)
                                    .ok_or(err(line, Reason::UnknownChannel))?;
                                let slot = &mut given[channel][which as usize];
                                if *slot {
                                    return Err(err(line, Reason::EndpointReused));
                                }
                                *slot = true;
                                Source::ChannelEnd {
                                    channel,
                                    end: which,
                                }
                            } else if let Some(path) = other.strip_prefix("file:") {
                                if !valid_path(path) {
                                    return Err(err(line, Reason::BadPath));
                                }
                                Source::File {
                                    path: String::from(path),
                                }
                            } else {
                                return Err(err(line, Reason::UnknownSource));
                            }
                        }
                    };
                    let slot = fields[2];
                    if !valid_name(slot.as_bytes()) {
                        return Err(err(line, Reason::BadName));
                    }
                    let rights = match fields.get(3) {
                        Some(f) => {
                            let r = parse_rights(f, line)?;
                            if !source_rights(&source).contains(r) {
                                return Err(err(line, Reason::BadRights));
                            }
                            Some(r)
                        }
                        None => None,
                    };
                    let process = manifest
                        .processes
                        .last_mut()
                        .ok_or(err(line, Reason::HandleBeforeProcess))?;
                    if source.is_root() && !supervisor {
                        return Err(err(line, Reason::RootOutsideSupervisor));
                    }
                    if process.handles.iter().any(|h| h.slot == slot) {
                        return Err(err(line, Reason::DuplicateSlot));
                    }
                    if process.handles.len() == crate::limits::START_HANDLES_MAX {
                        return Err(err(line, Reason::TooManyHandles));
                    }
                    process.handles.push(HandleSpec {
                        source,
                        slot: String::from(slot),
                        rights,
                    });
                }
                _ => return Err(err(line, Reason::UnknownDirective)),
            }
        }
        if !header {
            return Err(err(0, Reason::MissingHeader));
        }
        if manifest.processes.is_empty() {
            return Err(err(0, Reason::NoProcesses));
        }
        if given.iter().any(|ends| !ends[0] || !ends[1]) {
            return Err(err(0, Reason::EndpointUnused));
        }
        Ok(manifest)
    }
}

impl fmt::Display for Manifest {
    /// Renders the manifest in the format [`Manifest::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "moruna-manifest {VERSION}")?;
        for c in &self.channels {
            writeln!(f, "channel {c}")?;
        }
        for p in &self.processes {
            writeln!(f, "process {} {}", p.name, p.path)?;
            for h in &p.handles {
                f.write_str("handle ")?;
                match &h.source {
                    Source::Console => f.write_str("console")?,
                    Source::RootProcess => f.write_str("root.process")?,
                    Source::RootDevice => f.write_str("root.device")?,
                    Source::RootInterrupt => f.write_str("root.interrupt")?,
                    Source::RootEcam => f.write_str("root.ecam")?,
                    Source::RootMsi => f.write_str("root.msi")?,
                    Source::RootDma => f.write_str("root.dma")?,
                    Source::ChannelEnd { channel, end } => {
                        let name = self.channels.get(*channel).ok_or(fmt::Error)?;
                        write!(f, "channel:{name}.{end}")?;
                    }
                    Source::File { path } => write!(f, "file:{path}")?,
                }
                write!(f, " {}", h.slot)?;
                if let Some(r) = h.rights {
                    f.write_str(" rights=")?;
                    let mut first = true;
                    for (name, bit) in Rights::NAMES {
                        if r.contains(bit) {
                            if !first {
                                f.write_str(",")?;
                            }
                            f.write_str(name)?;
                            first = false;
                        }
                    }
                }
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::ToString};

    use super::*;

    const GOOD: &str = "\
# the KB1 boot
moruna-manifest 1
channel pp
process supervisor bin/supervisor
handle console console
handle root.process root.process
handle file:bin/child child-image
process ping bin/ping
handle console console
handle channel:pp.0 peer rights=read,write,wait
process pong bin/pong   # trailing comment
handle channel:pp.1 peer
";

    fn reason(text: &str) -> Reason {
        match Manifest::parse(text.as_bytes()) {
            Ok(m) => panic!("accepted: {m:?}"),
            Err(e) => e.reason,
        }
    }

    #[test]
    fn kk_t2_manifest_parses() {
        let m = Manifest::parse(GOOD.as_bytes()).expect("parses");
        assert_eq!(m.channels, ["pp"]);
        assert_eq!(m.processes.len(), 3);
        assert_eq!(m.processes[0].name, "supervisor");
        assert_eq!(m.processes[0].handles[1].source, Source::RootProcess);
        assert_eq!(
            m.processes[0].handles[2].source,
            Source::File {
                path: "bin/child".to_string()
            }
        );
        assert_eq!(
            m.processes[1].handles[1].rights,
            Some(Rights::READ | Rights::WRITE | Rights::WAIT)
        );
        assert_eq!(
            m.processes[2].handles[0].source,
            Source::ChannelEnd { channel: 0, end: 1 }
        );
    }

    #[test]
    fn kk_t2_manifest_round_trips() {
        let m = Manifest::parse(GOOD.as_bytes()).expect("parses");
        let text = m.to_string();
        assert_eq!(Manifest::parse(text.as_bytes()), Ok(m));
    }

    #[test]
    fn kk_t2_manifest_refusals() {
        let head = "moruna-manifest 1\n";
        assert_eq!(reason(""), Reason::MissingHeader);
        assert_eq!(reason("process a b\n"), Reason::MissingHeader);
        assert_eq!(reason("moruna-manifest 2\n"), Reason::BadVersion);
        assert_eq!(reason("moruna-manifest\n"), Reason::WrongArity);
        assert_eq!(reason(head), Reason::NoProcesses);
        assert_eq!(
            reason(&format!("{head}spawn a b\n")),
            Reason::UnknownDirective
        );
        assert_eq!(reason(&format!("{head}process A bin/a\n")), Reason::BadName);
        assert_eq!(
            reason(&format!("{head}process a /bin/a\n")),
            Reason::BadPath
        );
        assert_eq!(
            reason(&format!("{head}process a bin/../a\n")),
            Reason::BadPath
        );
        assert_eq!(
            reason(&format!("{head}process a bin//a\n")),
            Reason::BadPath
        );
        assert_eq!(reason(&format!("{head}process a\n")), Reason::WrongArity);
        assert_eq!(
            reason(&format!("{head}process a bin/a\nprocess a bin/b\n")),
            Reason::DuplicateProcess
        );
        assert_eq!(
            reason(&format!("{head}handle console c\n")),
            Reason::HandleBeforeProcess
        );
        assert_eq!(
            reason(&format!("{head}process a bin/a\nhandle magic c\n")),
            Reason::UnknownSource
        );
        assert_eq!(
            reason(&format!("{head}process a bin/a\nhandle channel:x.0 c\n")),
            Reason::UnknownChannel
        );
        assert_eq!(
            reason(&format!(
                "{head}channel x\nprocess a bin/a\nhandle channel:x.2 c\n"
            )),
            Reason::UnknownSource
        );
        assert_eq!(
            reason(&format!(
                "{head}channel x\nprocess a bin/a\nhandle channel:x.0 c\nhandle channel:x.0 d\n"
            )),
            Reason::EndpointReused
        );
        assert_eq!(
            reason(&format!(
                "{head}channel x\nprocess a bin/a\nhandle channel:x.0 c\n"
            )),
            Reason::EndpointUnused
        );
        assert_eq!(
            reason(&format!("{head}channel x\nchannel x\n")),
            Reason::DuplicateChannel
        );
        assert_eq!(
            reason(&format!(
                "{head}process a bin/a\nprocess b bin/b\nhandle root.device d\n"
            )),
            Reason::RootOutsideSupervisor
        );
        assert_eq!(
            reason(&format!(
                "{head}process a bin/a\nhandle console c\nhandle console c\n"
            )),
            Reason::DuplicateSlot
        );
        assert_eq!(
            reason(&format!(
                "{head}process a bin/a\nhandle console c rights=read\n"
            )),
            Reason::BadRights
        );
        assert_eq!(
            reason(&format!(
                "{head}process a bin/a\nhandle console c rights=fly\n"
            )),
            Reason::BadRights
        );
        assert_eq!(
            reason(&format!("{head}process a bin/a\nhandle console c write\n")),
            Reason::BadRights
        );
        assert_eq!(
            reason(&format!("{head}process a bin/a\nhandle file:/x c\n")),
            Reason::BadPath
        );
        assert_eq!(
            reason(&format!("{head}process a bin/a\nhandle console\n")),
            Reason::WrongArity
        );
        assert_eq!(reason(&format!("{head}channel\n")), Reason::WrongArity);
        assert_eq!(reason(&format!("{head}channel Bad\n")), Reason::BadName);
        assert_eq!(reason("moruna-manifest 1\u{0}\n"), Reason::NotText);
        assert_eq!(reason("moruna-manifest 1 \u{e9}\n"), Reason::NotText);
        let long = format!("{head}# {}\n", "x".repeat(MAX_LINE));
        assert_eq!(reason(&long), Reason::LineTooLong);
        let big = "#\n".repeat(MAX_BYTES / 2 + 1);
        assert_eq!(reason(&big), Reason::TooLarge);
    }

    #[test]
    fn kk_t2_manifest_bounds() {
        let mut text = String::from("moruna-manifest 1\n");
        for i in 0..=MAX_PROCESSES {
            text.push_str(&format!("process p{i} bin/p\n"));
        }
        assert_eq!(reason(&text), Reason::TooManyProcesses);

        let mut text = String::from("moruna-manifest 1\n");
        for i in 0..=MAX_CHANNELS {
            text.push_str(&format!("channel c{i}\n"));
        }
        assert_eq!(reason(&text), Reason::TooManyChannels);

        let mut text = String::from("moruna-manifest 1\nprocess a bin/a\n");
        for i in 0..=crate::limits::START_HANDLES_MAX {
            text.push_str(&format!("handle console c{i}\n"));
        }
        assert_eq!(reason(&text), Reason::TooManyHandles);
    }

    #[test]
    fn kk_t2_manifest_describes_every_refusal() {
        let e = Manifest::parse(b"moruna-manifest 9\n").expect_err("refused");
        assert_eq!(
            e.to_string(),
            "manifest line 1: the manifest version is not 1"
        );
        assert!(Source::RootInterrupt.is_root());
        assert!(!Source::Console.is_root());
        for r in [
            Reason::TooLarge,
            Reason::LineTooLong,
            Reason::NotText,
            Reason::MissingHeader,
            Reason::BadVersion,
            Reason::UnknownDirective,
            Reason::WrongArity,
            Reason::BadName,
            Reason::BadPath,
            Reason::DuplicateChannel,
            Reason::DuplicateProcess,
            Reason::DuplicateSlot,
            Reason::HandleBeforeProcess,
            Reason::UnknownSource,
            Reason::UnknownChannel,
            Reason::EndpointReused,
            Reason::EndpointUnused,
            Reason::RootOutsideSupervisor,
            Reason::BadRights,
            Reason::TooManyProcesses,
            Reason::TooManyChannels,
            Reason::TooManyHandles,
            Reason::NoProcesses,
        ] {
            assert!(!r.describe().is_empty());
        }
    }

    #[test]
    fn kk_t2_manifest_renders_rights_and_roots() {
        let text = "moruna-manifest 1\nprocess s bin/s\nhandle root.device d rights=write,transfer\nhandle root.interrupt i\n";
        let m = Manifest::parse(text.as_bytes()).expect("parses");
        let out = m.to_string();
        assert!(out.contains("handle root.device d rights=write,transfer\n"));
        assert!(out.contains("handle root.interrupt i\n"));
        assert_eq!(Manifest::parse(out.as_bytes()), Ok(m));
    }

    #[test]
    fn dr_t2_manifest_driver_roots() {
        let text = "moruna-manifest 1\nprocess s bin/s\nhandle root.ecam e\nhandle root.msi m\nhandle root.dma d rights=write,duplicate\n";
        let m = Manifest::parse(text.as_bytes()).expect("parses");
        let sources: Vec<&Source> = m.processes[0].handles.iter().map(|h| &h.source).collect();
        assert_eq!(
            sources,
            [&Source::RootEcam, &Source::RootMsi, &Source::RootDma]
        );
        for source in sources {
            assert!(source.is_root());
            assert_eq!(
                source_rights(source),
                Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER
            );
        }
        let out = m.to_string();
        assert!(out.contains("handle root.ecam e\n"));
        assert!(out.contains("handle root.msi m\n"));
        assert!(out.contains("handle root.dma d rights=write,duplicate\n"));
        assert_eq!(Manifest::parse(out.as_bytes()), Ok(m));
        for root in ["root.ecam", "root.msi", "root.dma"] {
            assert_eq!(
                reason(&format!(
                    "moruna-manifest 1\nprocess a bin/a\nprocess b bin/b\nhandle {root} r\n"
                )),
                Reason::RootOutsideSupervisor
            );
        }
    }
}
