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
}
