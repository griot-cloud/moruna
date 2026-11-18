//! Volumes (`architecture/moruna-os-system.md` 3C, item 3.3): the disks attached to a node,
//! which a job names by the serial the cloud gives each (`disk://<serial>/<path>`), each
//! served by its driver through `moruna.driver.block`, virtio-blk's or storvsc's alike. The
//! node grants the engine a connection to each volume its job names, read only for a source and
//! read-write for a sink, and the driver refuses a source's writes below the engine.
//!
//! - [`io`]: a volume's bytes through its driver ([`io::Driver`]), with no copy: a read's whole
//!   sectors land in the reader's memory (`read_to`), only its edge sectors pass through a
//!   bounce, its requests are as large as the driver takes and all on their way at once;
//! - [`layer`]: the files of the layer the volume holds, its index checked against the layer's
//!   hashes, a file's bytes read straight from the volume;
//! - [`block`] (on the kernel): the driver's client, over a connection the node granted.
//!
//! Everything but the client is the same code on every host, where it is tested against a
//! driver in memory.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod block;
pub mod io;
pub mod layer;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

/// The URL scheme a job names a volume's file with.
pub const SCHEME: &str = "disk://";

/// The start slot of the data `node-admission` grants a job's engine with its volumes.
pub const GRANTS_SLOT: &str = "volumes";

/// One volume the node granted the engine: the slot of the connection to its driver, the serial
/// a job names it by, and whether it is a sink's, which the job writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The slot: `volume.<n>`.
    pub slot: String,
    /// Its serial.
    pub serial: String,
    /// A sink's volume, written; a source's otherwise, read only.
    pub sink: bool,
}

/// The volumes `node-admission` granted, from its list ([`GRANTS_SLOT`]): a line each, `<n>
/// <serial> <source|sink>` (`node_protocol::volume_grants`), the connection's slot
/// `volume.<n>`.
pub fn parse_grants(text: &str) -> Result<Vec<Grant>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let bad = || format!("the node's volume grants: `{line}`");
            match line.split_ascii_whitespace().collect::<Vec<_>>().as_slice() {
                [n, serial, role] => {
                    let n: u32 = n.parse().map_err(|_| bad())?;
                    let sink = match *role {
                        "sink" => true,
                        "source" => false,
                        _ => return Err(bad()),
                    };
                    Ok(Grant {
                        slot: format!("volume.{n}"),
                        serial: (*serial).to_string(),
                        sink,
                    })
                }
                _ => Err(bad()),
            }
        })
        .collect()
}

/// The serial and the path a `disk://<serial>/<path>` URL names; the path may be empty (a
/// listing of the volume). `None` for another URL, or one with no serial.
pub fn parse(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix(SCHEME)?;
    let (serial, path) = rest.split_once('/')?;
    (!serial.is_empty()).then_some((serial, path))
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn a_url_names_a_serial_and_a_path() {
        assert_eq!(
            parse("disk://vol-1a2b/nyc/2024-01.parquet"),
            Some(("vol-1a2b", "nyc/2024-01.parquet"))
        );
        assert_eq!(parse("disk://lun-0/"), Some(("lun-0", "")));
        assert_eq!(parse("disk://lun-0"), None, "no path at all");
        assert_eq!(parse("disk:///x"), None, "no serial");
        assert_eq!(parse("s3://bucket/x"), None);
    }

    #[test]
    fn the_nodes_grants_name_each_volumes_slot_serial_and_role() {
        // As node_protocol::volume_grants writes them.
        assert_eq!(
            parse_grants("0 vol-in source\n1 vol-out sink\n").unwrap(),
            [
                Grant {
                    slot: "volume.0".into(),
                    serial: "vol-in".into(),
                    sink: false
                },
                Grant {
                    slot: "volume.1".into(),
                    serial: "vol-out".into(),
                    sink: true
                },
            ]
        );
        assert_eq!(parse_grants("").unwrap(), []);
        for bad in ["x vol source", "0 vol", "0 vol both", "0 vol sink more"] {
            assert!(parse_grants(bad).is_err(), "{bad}");
        }
    }
}
