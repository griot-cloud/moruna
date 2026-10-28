//! The driver table (SDD 25, e.3): which program drives which PCI device, and the name
//! its service is published under. It is the extension point for drivers: a third
//! party's driver is one more line, and nothing in the kernel changes.
//!
//! ```text
//! moruna-drivers 1
//! driver 1af4:1042 virtio-blk block
//! ```
//!
//! A line names a PCI vendor and device id (four hex digits each), the driver program
//! (its image is in the device manager's slot `driver.PROGRAM`), and the service name:
//! the device manager publishes the n-th device a line matches, in PCI order, as
//! `SERVICEn` (`block0`, `block1`). `#` begins a comment line. The boot image tool
//! renders the table from its description's `driver` lines; the device manager parses
//! it with this code.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
use core::fmt;

use moruna_abi::valid_name;

/// The format version this code reads and writes.
pub const VERSION: &str = "1";
/// The prefix of the device manager's slot holding a driver program's image.
pub const IMAGE_SLOT: &str = "driver.";
/// The longest program name: the image slot name stays within 32 bytes.
pub const PROGRAM_MAX: usize = moruna_abi::limits::NAME_MAX - IMAGE_SLOT.len();
/// The longest service name: room for a two-digit index in a process name.
pub const SERVICE_MAX: usize = moruna_abi::limits::NAME_MAX - 2;
/// The most lines one table holds.
pub const MAX_DRIVERS: usize = 32;

/// One line: devices with this vendor and device id are driven by `program` and
/// published as `service` and an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Driver {
    pub vendor: u16,
    pub device: u16,
    pub program: String,
    pub service: String,
}

impl Driver {
    /// The slot of the device manager that holds this driver's image.
    pub fn image_slot(&self) -> String {
        format!("{IMAGE_SLOT}{}", self.program)
    }
}

/// A parsed table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub drivers: Vec<Driver>,
}

/// A refusal, and its line (1-based; 0 for the whole table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableError {
    pub line: usize,
    pub reason: &'static str,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "driver table line {}: {}", self.line, self.reason)
    }
}

fn err<T>(line: usize, reason: &'static str) -> Result<T, TableError> {
    Err(TableError { line, reason })
}

/// Parses `VVVV:DDDD`, four lower or upper case hex digits each.
pub fn parse_ids(field: &str) -> Option<(u16, u16)> {
    let (v, d) = field.split_once(':')?;
    let hex = |s: &str| {
        (s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u16::from_str_radix(s, 16).ok())
            .flatten()
    };
    Some((hex(v)?, hex(d)?))
}

/// Whether `name` is a valid program name: a process name short enough for its slot.
pub fn valid_program(name: &str) -> bool {
    name.len() <= PROGRAM_MAX && valid_name(name.as_bytes())
}

/// Whether `name` is a valid service name: a process name with room for an index.
pub fn valid_service(name: &str) -> bool {
    name.len() <= SERVICE_MAX && valid_name(name.as_bytes())
}

impl Table {
    /// Parses a table; every refusal names its line.
    pub fn parse(text: &str) -> Result<Table, TableError> {
        let mut table = Table::default();
        let mut header = false;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.trim();
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = content.split_whitespace().collect();
            if !header {
                if fields != ["moruna-drivers", VERSION] {
                    return err(line, "the first line must be `moruna-drivers 1`");
                }
                header = true;
                continue;
            }
            let ["driver", ids, program, service] = fields.as_slice() else {
                return err(line, "a line is `driver VVVV:DDDD PROGRAM SERVICE`");
            };
            let Some((vendor, device)) = parse_ids(ids) else {
                return err(line, "the ids are not VVVV:DDDD in hex");
            };
            if !valid_program(program) {
                return err(line, "the program is not 1 to 25 bytes of [a-z0-9._-]");
            }
            if !valid_service(service) {
                return err(line, "the service is not 1 to 30 bytes of [a-z0-9._-]");
            }
            if table
                .drivers
                .iter()
                .any(|d| d.vendor == vendor && d.device == device)
            {
                return err(line, "a device has two drivers");
            }
            if table.drivers.iter().any(|d| d.service == *service) {
                return err(line, "a service name is used by two lines");
            }
            if table.drivers.len() == MAX_DRIVERS {
                return err(line, "more than 32 drivers");
            }
            table.drivers.push(Driver {
                vendor,
                device,
                program: String::from(*program),
                service: String::from(*service),
            });
        }
        if !header {
            return err(0, "the table is empty; it starts with `moruna-drivers 1`");
        }
        Ok(table)
    }

    /// The line that drives a device with these ids, if any.
    pub fn find(&self, vendor: u16, device: u16) -> Option<&Driver> {
        self.drivers
            .iter()
            .find(|d| d.vendor == vendor && d.device == device)
    }

    /// The distinct programs the table names, in first-use order.
    pub fn programs(&self) -> Vec<&str> {
        let mut programs: Vec<&str> = Vec::new();
        for d in &self.drivers {
            if !programs.contains(&d.program.as_str()) {
                programs.push(&d.program);
            }
        }
        programs
    }
}

impl fmt::Display for Table {
    /// Renders the table in the format [`Table::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "moruna-drivers {VERSION}")?;
        for d in &self.drivers {
            writeln!(
                f,
                "driver {:04x}:{:04x} {} {}",
                d.vendor, d.device, d.program, d.service
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    const GOOD: &str = "\
# the CI boot's drivers
moruna-drivers 1
driver 1af4:1042 virtio-blk block
driver 1AF4:1043 virtio-console console

driver 1af4:1053 virtio-vsock vsock
driver 1af4:1044 virtio-blk entropy
";

    fn refused(text: &str) -> TableError {
        match Table::parse(text) {
            Ok(t) => panic!("accepted: {t:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn dr_t4_the_table_parses_renders_and_round_trips() {
        let t = Table::parse(GOOD).expect("parses");
        assert_eq!(t.drivers.len(), 4);
        assert_eq!(t.drivers[1].vendor, 0x1af4);
        assert_eq!(t.drivers[1].device, 0x1043);
        assert_eq!(t.drivers[0].image_slot(), "driver.virtio-blk");
        assert_eq!(
            t.find(0x1af4, 0x1053).map(|d| d.service.as_str()),
            Some("vsock")
        );
        assert_eq!(t.find(0x1af4, 0x1000), None);
        assert_eq!(
            t.programs(),
            ["virtio-blk", "virtio-console", "virtio-vsock"]
        );
        let text = t.to_string();
        assert!(text.contains("driver 1af4:1043 virtio-console console\n"));
        assert_eq!(Table::parse(&text), Ok(t));
    }

    #[test]
    fn dr_t4_the_table_refuses_by_line() {
        let head = "moruna-drivers 1\n";
        assert_eq!(refused("").line, 0);
        assert_eq!(refused("driver 1af4:1042 a b\n").line, 1);
        assert_eq!(refused("moruna-drivers 2\n").line, 1);
        for bad in [
            "driver 1af4:1042 a",
            "drive 1af4:1042 a b",
            "driver 1af4-1042 a b",
            "driver 1af:1042 a b",
            "driver 1af4:10g2 a b",
            "driver +af4:1042 a b",
            "driver 1af4:1042 Bad b",
            "driver 1af4:1042 a Bad",
            "driver 1af4:1042 a-program-name-of-26-bytes b",
            "driver 1af4:1042 a a-service-name-of-31-bytes-long",
        ] {
            let e = refused(&format!("{head}{bad}\n"));
            assert_eq!(e.line, 2, "{bad}");
            assert!(!e.reason.is_empty());
        }
        assert_eq!(
            refused(&format!(
                "{head}driver 1af4:1042 a b\ndriver 1af4:1042 c d\n"
            ))
            .reason,
            "a device has two drivers"
        );
        assert_eq!(
            refused(&format!(
                "{head}driver 1af4:1042 a b\ndriver 1af4:1043 c b\n"
            ))
            .reason,
            "a service name is used by two lines"
        );
        let mut many = String::from(head);
        for i in 0..=MAX_DRIVERS {
            many.push_str(&format!("driver 1af4:{i:04x} p s{i}\n"));
        }
        assert_eq!(refused(&many).reason, "more than 32 drivers");
        assert_eq!(
            refused("moruna-drivers 9\n").to_string(),
            "driver table line 1: the first line must be `moruna-drivers 1`"
        );
    }

    #[test]
    fn dr_t4_names_and_ids() {
        assert_eq!(parse_ids("1af4:1042"), Some((0x1af4, 0x1042)));
        assert_eq!(parse_ids("FFFF:0000"), Some((0xffff, 0)));
        assert_eq!(parse_ids("1af41042"), None);
        assert!(valid_program("virtio-blk"));
        assert!(!valid_program(""));
        assert!(valid_service("block"));
        assert!(!valid_service("Block"));
    }
}
