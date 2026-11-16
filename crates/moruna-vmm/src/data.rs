//! A run's data disks (`architecture/data-os.md` §3, contracts C3 and C7): each input of the job
//! a read-only virtio-blk device holding one file, and one writable output disk, attached beside
//! the image's own devices.
//!
//! The names, the device ids and the serial are the frozen contract between the launcher and the
//! engine (`moruna_micro::devices`):
//!
//! - a data disk is transitional virtio-blk, PCI 1af4:1001 (legacy and modern interfaces both
//!   on), so the guest tells it apart from the image's layers (1af4:1042). The node's device
//!   table starts no driver for it: the device manager names the data disks `data0`, `data1`,
//!   ... in PCI order and hands each to the engine's supervisor once;
//! - the image declares its inputs by name, in device order ([`DataDisks`]): input `i` is
//!   `data{i}`, granted to the engine as `disk.in.NAME`, and the output disk is the next one,
//!   granted as `disk.out`. A job names an input as `disk://NAME`;
//! - an input's serial is its file's exact length in decimal ASCII, because a virtio-blk
//!   capacity is whole sectors. The output disk has no serial;
//! - the output disk holds, when the run completes, one layer file from its first byte, which the
//!   host reads back into the job's output directory.
//!
//! Every data disk sits at a fixed PCI address from [`FIRST_SLOT`], eight functions to a slot
//! ([`address_of`]), so PCI order is the declared order whatever else the machine has; the device
//! manager scans every function. At most [`DISKS_MAX`] are attached: each is a handle on the
//! engine's start page (at most 64) and a grant line of its one-page configuration. Inputs are
//! opened by the launcher, held under a shared
//! lock for the VM's life and shown to QEMU's jail read-only; the output disk is created new by
//! the launcher, sparse, held under an exclusive lock, and is the only file of the run QEMU may
//! write besides its own logs and sockets.

use crate::error::{Result, VmmError};
use serde::Deserialize;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// The first PCI slot of a data disk on the root bus.
pub const FIRST_SLOT: u32 = 0x10;
/// The last slot a data disk may take (0x1f is the chipset's).
pub const LAST_SLOT: u32 = 0x1e;
/// The functions of one slot.
pub const FUNCTIONS: usize = 8;
/// The most data disks a run attaches, inputs and output together: room on the engine's start
/// page (64 handles, about a dozen of them the engine's own) and in its one-page configuration.
pub const DISKS_MAX: usize = 40;
/// The root bus's slots the data disks take: whole slots of [`FUNCTIONS`].
pub fn slots_for(disks: usize) -> usize {
    disks.div_ceil(FUNCTIONS)
}

/// The PCI address of data disk `i`: its slot, its function, and whether it is function 0 of a
/// slot that holds more than one (`multifunction=on`).
pub fn address_of(i: usize, disks: usize) -> (u32, u32, bool) {
    let slot = FIRST_SLOT + (i / FUNCTIONS) as u32;
    let function = (i % FUNCTIONS) as u32;
    let shared = disks > i - i % FUNCTIONS + 1;
    (slot, function, function == 0 && shared)
}

/// The longest input name (`moruna_micro::devices::NAME_MAX`): a start-page name is at most 32
/// bytes and `disk.in.` takes 8.
pub const NAME_MAX: usize = 24;
/// The longest serial virtio-blk carries (`moruna_micro::devices::SERIAL_MAX`).
pub const SERIAL_MAX: usize = 20;
/// The largest output disk an image may declare.
pub const OUTPUT_MAX: u64 = 1 << 40;
/// The PCI device id of a data disk: transitional virtio-blk (`DATA_DISK_DEVICE_ID`).
pub const DATA_DISK_DEVICE_ID: u16 = 0x1001;

/// The data disks an image is built for: its `node-image.json` section `data_disks`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataDisks {
    /// The inputs' names, in device order: input `i` is the guest's `data{i}` and the engine's
    /// `disk.in.NAME`.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The output disk's size in bytes, a whole number of pages; zero: the image has none.
    #[serde(default)]
    pub output_bytes: u64,
}

impl DataDisks {
    /// How many devices these are.
    pub fn count(&self) -> usize {
        self.inputs.len() + usize::from(self.output_bytes > 0)
    }

    /// Whether the section is one the guest can be built for: valid, distinct names, at most
    /// [`DISKS_MAX`] devices, and an output size in whole pages up to [`OUTPUT_MAX`].
    pub fn check(&self) -> Result<()> {
        if self.count() > DISKS_MAX {
            return Err(bad(format!(
                "{} data disks; a run attaches at most {DISKS_MAX}",
                self.count()
            )));
        }
        for (i, name) in self.inputs.iter().enumerate() {
            if !valid_name(name) {
                return Err(bad(format!(
                    "input name `{name}` is not one [a-z0-9._-] word of 1 to {NAME_MAX} bytes"
                )));
            }
            if self.inputs[..i].contains(name) {
                return Err(bad(format!("input `{name}` is declared twice")));
            }
        }
        if !self.output_bytes.is_multiple_of(4096) || self.output_bytes > OUTPUT_MAX {
            return Err(bad(format!(
                "an output disk of {} bytes; it is whole pages, at most {OUTPUT_MAX}",
                self.output_bytes
            )));
        }
        Ok(())
    }
}

/// Whether `name` may name an input: what `moruna_micro::devices::input_slot` accepts after
/// `disk://`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// The serial of an input holding `len` bytes (`moruna_micro::devices::serial_of`).
pub fn serial_of(len: u64) -> String {
    len.to_string()
}

/// One input a run binds: the image's name for it and the host file that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// The name the image declares, `disk://NAME` in the job.
    pub name: String,
    /// The host file, a regular file the launcher opens read-only.
    pub path: PathBuf,
}

/// What a run attaches beside its image: its inputs, its output disk and where its vCPUs run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunDevices {
    /// Every input the image declares, each once, in any order.
    pub inputs: Vec<Input>,
    /// Where the launcher creates the output disk: a path that does not exist yet, outside the
    /// run's directory, so it outlives the VM for the host to read. Required when the image
    /// declares an output disk, refused otherwise.
    pub output: Option<PathBuf>,
    /// The host CPU each vCPU runs on, vCPU 0 first (`crate::placement`); empty leaves them
    /// to the host's scheduler.
    pub vcpu_cpus: Vec<u32>,
}

/// A data disk opened for one VM: its file, held under its lock until the VM is gone.
#[derive(Debug)]
pub struct Opened {
    /// The canonical path QEMU opens and its jail shows.
    pub path: PathBuf,
    /// The serial, for an input.
    pub serial: Option<String>,
    /// Whether QEMU may write it.
    pub writable: bool,
    /// The open file whose lock keeps the disk the run's.
    pub lease: File,
}

fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("data disks", message)
}

/// A path QEMU can take inside a `-drive` option: absolute, and without the characters that
/// end an option (`,`) or a line.
fn safe_for_qemu(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| matches!(b, b',' | b'\n' | b'\r'))
    {
        return Err(bad(format!(
            "{} must be an absolute path without `,` or line breaks",
            path.display()
        )));
    }
    Ok(())
}

/// The inputs in the image's order, each declared name bound exactly once, and the output disk
/// given exactly when the image declares one.
pub fn bind<'a>(declared: &DataDisks, devices: &'a RunDevices) -> Result<Vec<&'a Input>> {
    let mut ordered = Vec::with_capacity(declared.inputs.len());
    for name in &declared.inputs {
        let mut bound = devices.inputs.iter().filter(|i| &i.name == name);
        let input = bound.next().ok_or_else(|| {
            bad(format!(
                "the image's input `{name}` is bound to no file (`--input {name}=PATH`)"
            ))
        })?;
        if bound.next().is_some() {
            return Err(bad(format!("input `{name}` is bound twice")));
        }
        ordered.push(input);
    }
    if let Some(extra) = devices
        .inputs
        .iter()
        .find(|i| !declared.inputs.contains(&i.name))
    {
        return Err(bad(format!(
            "input `{}` is not one the image declares ({})",
            extra.name,
            if declared.inputs.is_empty() {
                "it declares none".to_string()
            } else {
                declared.inputs.join(", ")
            }
        )));
    }
    match (declared.output_bytes > 0, &devices.output) {
        (true, None) => Err(bad(
            "the image declares an output disk, and the run names no output",
        )),
        (false, Some(_)) => Err(bad(
            "the run names an output, and the image declares no output disk",
        )),
        _ => Ok(ordered),
    }
}

/// Open input `input` read-only for the VM: a regular file reached without a symbolic link at
/// its last component, held under a shared lock (which still refuses a writer that locks), its
/// length the serial.
pub fn open_input(input: &Input) -> Result<Opened> {
    safe_for_qemu(&input.path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&input.path)
        .map_err(|e| VmmError::io("open input", input.path.display().to_string(), &e))?;
    let metadata = file
        .metadata()
        .map_err(|e| VmmError::io("metadata", input.path.display().to_string(), &e))?;
    if !metadata.is_file() {
        return Err(bad(format!(
            "input `{}` ({}) is not a regular file",
            input.name,
            input.path.display()
        )));
    }
    let path = input
        .path
        .canonicalize()
        .map_err(|e| VmmError::io("resolve", input.path.display().to_string(), &e))?;
    safe_for_qemu(&path)?;
    let named = std::fs::metadata(&path)
        .map_err(|e| VmmError::io("metadata", path.display().to_string(), &e))?;
    if (named.dev(), named.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(bad(format!(
            "input `{}` changed while it was opened",
            input.name
        )));
    }
    file.try_lock_shared()
        .map_err(|e| bad(format!("input `{}` is locked by a writer: {e}", input.name)))?;
    Ok(Opened {
        path,
        serial: Some(serial_of(metadata.len())),
        writable: false,
        lease: file,
    })
}

/// Create the output disk at `path`: a new file, sparse, `bytes` long, under an exclusive lock.
pub fn create_output(path: &Path, bytes: u64) -> Result<Opened> {
    safe_for_qemu(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| VmmError::io("create output disk", path.display().to_string(), &e))?;
    let path = path
        .canonicalize()
        .map_err(|e| VmmError::io("resolve", path.display().to_string(), &e))?;
    safe_for_qemu(&path)?;
    file.set_len(bytes)
        .map_err(|e| VmmError::io("size output disk", path.display().to_string(), &e))?;
    file.try_lock()
        .map_err(|e| bad(format!("the output disk is already owned: {e}")))?;
    Ok(Opened {
        path,
        serial: None,
        writable: true,
        lease: file,
    })
}

/// Open every data disk of a run, inputs in the image's order and then the output disk.
pub fn open(declared: &DataDisks, devices: &RunDevices) -> Result<Vec<Opened>> {
    declared.check()?;
    let inputs = bind(declared, devices)?;
    let mut opened = Vec::with_capacity(declared.count());
    for input in inputs {
        opened.push(open_input(input)?);
    }
    if let Some(path) = &devices.output {
        opened.push(create_output(path, declared.output_bytes)?);
    }
    Ok(opened)
}

/// QEMU's arguments for the data disks, in device order: each a raw drive and a transitional
/// virtio-blk function at its fixed slot, read-only with its serial for an input.
pub fn qemu_args(disks: &[Opened]) -> Result<Vec<String>> {
    if disks.len() > DISKS_MAX {
        return Err(bad(format!(
            "{} data disks; a run attaches at most {DISKS_MAX}",
            disks.len()
        )));
    }
    let mut args = Vec::with_capacity(disks.len() * 4);
    for (i, disk) in disks.iter().enumerate() {
        safe_for_qemu(&disk.path)?;
        let readonly = if disk.writable { "" } else { ",readonly=on" };
        args.push("-drive".to_string());
        args.push(format!(
            "file={},format=raw,if=none,id=data{i}{readonly}",
            disk.path.display()
        ));
        let serial = match &disk.serial {
            Some(serial)
                if !serial.is_empty()
                    && serial.len() <= SERIAL_MAX
                    && serial.bytes().all(|b| b.is_ascii_digit()) =>
            {
                format!(",serial={serial}")
            }
            Some(serial) => return Err(bad(format!("`{serial}` is not a length serial"))),
            None => String::new(),
        };
        let (slot, function, multifunction) = address_of(i, disks.len());
        args.push("-device".to_string());
        args.push(format!(
            "virtio-blk-pci,drive=data{i},id=data-disk{i},addr=0x{slot:x}.0x{function:x}{},disable-legacy=off,disable-modern=off{serial}",
            if multifunction { ",multifunction=on" } else { "" }
        ));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("moruna-data-{tag}-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Scratch(path.canonicalize().unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn declared(inputs: &[&str], output_bytes: u64) -> DataDisks {
        DataDisks {
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            output_bytes,
        }
    }

    fn input(name: &str, path: &Path) -> Input {
        Input {
            name: name.into(),
            path: path.into(),
        }
    }

    #[test]
    fn a_descriptor_section_reads_and_is_checked() {
        let read: DataDisks =
            serde_json::from_str(r#"{"inputs": ["nyc", "zones"], "output_bytes": 8192}"#).unwrap();
        assert_eq!(read, declared(&["nyc", "zones"], 8192));
        assert_eq!(read.count(), 3);
        read.check().unwrap();
        assert_eq!(
            serde_json::from_str::<DataDisks>("{}").unwrap(),
            DataDisks::default()
        );
        assert!(serde_json::from_str::<DataDisks>(r#"{"inputs": [], "extra": 1}"#).is_err());
        let long = "a".repeat(25);
        for (bad, why) in [
            (declared(&["Upper"], 0), "is not one"),
            (declared(&[""], 0), "is not one"),
            (declared(&[long.as_str()], 0), "is not one"),
            (declared(&["a/b"], 0), "is not one"),
            (declared(&["a", "a"], 0), "declared twice"),
            (declared(&[], 4095), "whole pages"),
            (declared(&[], OUTPUT_MAX + 4096), "whole pages"),
        ] {
            let e = bad.check().unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
        }
        let names: Vec<String> = (0..DISKS_MAX).map(|i| format!("in{i}")).collect();
        let full = DataDisks {
            inputs: names.clone(),
            output_bytes: 0,
        };
        full.check().unwrap();
        let over = DataDisks {
            inputs: names,
            output_bytes: 4096,
        };
        assert!(over.check().unwrap_err().to_string().contains("at most 40"));
        assert!(valid_name(&"a".repeat(NAME_MAX)));
        assert!(valid_name("nyc-2024.v1_a"));
    }

    #[test]
    fn disks_take_eight_functions_to_a_slot_in_order() {
        assert_eq!(address_of(0, 1), (0x10, 0, false));
        assert_eq!(address_of(0, 2), (0x10, 0, true));
        assert_eq!(address_of(7, 9), (0x10, 7, false));
        assert_eq!(address_of(8, 9), (0x11, 0, false), "alone in its slot");
        assert_eq!(address_of(8, 10), (0x11, 0, true));
        assert_eq!(address_of(DISKS_MAX - 1, DISKS_MAX), (0x14, 7, false));
        assert_eq!(slots_for(0), 0);
        assert_eq!(slots_for(1), 1);
        assert_eq!(slots_for(8), 1);
        assert_eq!(slots_for(9), 2);
        assert!(FIRST_SLOT as usize + slots_for(DISKS_MAX) - 1 <= LAST_SLOT as usize);
    }

    #[test]
    fn the_serial_is_the_length_in_decimal() {
        assert_eq!(serial_of(0), "0");
        assert_eq!(serial_of(123_456), "123456");
        assert_eq!(serial_of(u64::MAX).len(), SERIAL_MAX);
        assert_eq!(DATA_DISK_DEVICE_ID, 0x1001);
    }

    #[test]
    fn bindings_name_every_declared_input_once() {
        let d = declared(&["a", "b"], 4096);
        let devices = RunDevices {
            inputs: vec![input("b", Path::new("/b")), input("a", Path::new("/a"))],
            output: Some("/out".into()),
            vcpu_cpus: vec![],
        };
        let ordered = bind(&d, &devices).unwrap();
        assert_eq!(
            ordered.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"],
            "the image's order, not the command line's"
        );
        let refusal =
            |devices: &RunDevices, d: &DataDisks| bind(d, devices).unwrap_err().to_string();
        let mut missing = devices.clone();
        missing.inputs.pop();
        assert!(refusal(&missing, &d).contains("`a` is bound to no file"));
        let mut twice = devices.clone();
        twice.inputs.push(input("a", Path::new("/c")));
        assert!(refusal(&twice, &d).contains("bound twice"));
        let mut extra = devices.clone();
        extra.inputs.push(input("c", Path::new("/c")));
        assert!(refusal(&extra, &d).contains("`c` is not one the image declares (a, b)"));
        let mut no_output = devices.clone();
        no_output.output = None;
        assert!(refusal(&no_output, &d).contains("names no output"));
        let none = declared(&[], 0);
        assert!(refusal(&devices, &none).contains("it declares none"));
        let plain = RunDevices {
            output: Some("/out".into()),
            ..RunDevices::default()
        };
        assert!(refusal(&plain, &none).contains("declares no output disk"));
        assert!(bind(&none, &RunDevices::default()).unwrap().is_empty());
    }

    #[test]
    fn inputs_open_read_only_with_their_length_and_the_output_is_created_new() {
        let s = Scratch::new("open");
        let file = s.0.join("x.parquet");
        std::fs::write(&file, vec![7u8; 1000]).unwrap();
        let d = declared(&["x"], 1 << 20);
        let out = s.0.join("out.disk");
        let devices = RunDevices {
            inputs: vec![input("x", &file)],
            output: Some(out.clone()),
            vcpu_cpus: vec![],
        };
        let opened = open(&d, &devices).unwrap();
        assert_eq!(opened.len(), 2);
        assert_eq!(opened[0].serial.as_deref(), Some("1000"));
        assert!(!opened[0].writable);
        assert_eq!(opened[0].path, file);
        assert!(opened[1].writable);
        assert_eq!(opened[1].serial, None);
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 1 << 20);
        // The output disk is never reused: a second creation at its path is refused.
        assert!(create_output(&out, 4096).is_err());
        let args = qemu_args(&opened).unwrap();
        assert_eq!(
            args,
            [
                "-drive".to_string(),
                format!(
                    "file={},format=raw,if=none,id=data0,readonly=on",
                    file.display()
                ),
                "-device".to_string(),
                "virtio-blk-pci,drive=data0,id=data-disk0,addr=0x10.0x0,multifunction=on,disable-legacy=off,disable-modern=off,serial=1000"
                    .to_string(),
                "-drive".to_string(),
                format!("file={},format=raw,if=none,id=data1", out.display()),
                "-device".to_string(),
                "virtio-blk-pci,drive=data1,id=data-disk1,addr=0x10.0x1,disable-legacy=off,disable-modern=off"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn an_input_that_is_not_a_plain_file_is_refused() {
        let s = Scratch::new("refuse");
        let file = s.0.join("real");
        std::fs::write(&file, b"abc").unwrap();
        let link = s.0.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_input(&input("x", &link)).is_err(), "no symbolic link");
        assert!(open_input(&input("x", &s.0)).is_err(), "no directory");
        assert!(open_input(&input("x", &s.0.join("absent"))).is_err());
        assert!(open_input(&input("x", Path::new("relative"))).is_err());
        let comma = s.0.join("a,b");
        std::fs::write(&comma, b"abc").unwrap();
        let e = open_input(&input("x", &comma)).unwrap_err().to_string();
        assert!(e.contains("without `,`"), "{e}");
        // A writer holding the file's lock keeps it from a run.
        let writer = OpenOptions::new().write(true).open(&file).unwrap();
        writer.try_lock().unwrap();
        let e = open_input(&input("x", &file)).unwrap_err().to_string();
        assert!(e.contains("locked by a writer"), "{e}");
        drop(writer);
        assert_eq!(
            open_input(&input("x", &file)).unwrap().serial.as_deref(),
            Some("3")
        );
        assert!(create_output(&s.0.join("rel,ative"), 4096).is_err());
        assert!(
            create_output(&file, 4096).is_err(),
            "never an existing file"
        );
    }

    #[test]
    fn qemu_arguments_refuse_what_they_cannot_carry() {
        let s = Scratch::new("args");
        let lease = File::create(s.0.join("f")).unwrap();
        let disk = |serial: Option<&str>| Opened {
            path: s.0.join("f"),
            serial: serial.map(str::to_string),
            writable: false,
            lease: lease.try_clone().unwrap(),
        };
        for serial in ["", "12a", "123456789012345678901"] {
            assert!(qemu_args(&[disk(Some(serial))]).is_err(), "{serial:?}");
        }
        let many: Vec<Opened> = (0..=DISKS_MAX).map(|_| disk(Some("1"))).collect();
        assert!(qemu_args(&many).is_err());
        let last = qemu_args(&many[..DISKS_MAX]).unwrap();
        assert!(last.last().unwrap().contains("addr=0x14.0x7,"), "{last:?}");
        let one = qemu_args(&many[..1]).unwrap();
        assert!(one[3].contains("addr=0x10.0x0,disable"), "one disk alone: {one:?}");
        assert!(qemu_args(&[]).unwrap().is_empty());
        let relative = Opened {
            path: PathBuf::from("f"),
            serial: None,
            writable: true,
            lease: lease.try_clone().unwrap(),
        };
        assert!(qemu_args(&[relative]).is_err());
    }
}
