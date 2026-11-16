//! The data devices' names: the contract between the launcher (stream D) and the engine
//! (stream C), frozen for the parallel streams (`architecture/data-os-streams.md`).
//!
//! The launcher attaches each input of a job as a read-only virtio-blk device and the run's
//! output disk as a writable one, and the engine's supervisor grants them to the engine under
//! these start-page names. A job names an input as `disk://NAME`, meaning the whole device
//! `disk.in.NAME` holds one file. The device's virtio-blk serial is that file's exact length in
//! decimal ASCII, because its capacity is whole sectors (stream C's item 2, 2026-11-16).
//!
//! The launcher attaches data disks as transitional virtio-blk (PCI 1af4:1001, modern
//! capabilities on), so they are told apart from the image layers' disks (1af4:1042). Their
//! device-table lines start no driver: the device manager hands each one's resources to the
//! engine's supervisor once (`hand_over`, stream C), which grants them to the engine under
//! these names. The output disk holds, at the run's end, one layer file in
//! `moruna-image`'s format of the sinks' files, which the host reads with `moruna-layer`
//! (`architecture/arrow-core-0.7.md` decision 5, SDD 30 m.3).

/// The start-page name of input `NAME` is this prefix followed by `NAME`.
pub const INPUT_PREFIX: &str = "disk.in.";
/// The start-page name of the run's output disk.
pub const OUTPUT: &str = "disk.out";
/// The URL scheme a job uses for an input device.
pub const SCHEME: &str = "disk://";
/// The longest `NAME`: a start-page name is at most 32 bytes, and `INPUT_PREFIX` takes 8.
pub const NAME_MAX: usize = 24;
/// The PCI device id of a data disk: transitional virtio-blk.
pub const DATA_DISK_DEVICE_ID: u16 = 0x1001;
/// The longest serial virtio-blk carries.
pub const SERIAL_MAX: usize = 20;

/// The serial the launcher gives an input device holding a file of `len` bytes.
pub fn serial_of(len: u64) -> String {
    len.to_string()
}

/// The file length a device's serial states, or `None` for an empty or malformed serial, in
/// which case the length is the device's capacity. A serial is padded with NULs to 20 bytes.
pub fn length_of(serial: &[u8]) -> Option<u64> {
    let end = serial.iter().position(|&b| b == 0).unwrap_or(serial.len());
    let text = core::str::from_utf8(&serial[..end]).ok()?;
    if text.is_empty() || text.len() > SERIAL_MAX || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The start-page name of the input a `disk://NAME` URL names, or `None` for another URL or
/// a name that is not one `[a-z0-9._-]{1,24}` word.
pub fn input_slot(url: &str) -> Option<String> {
    let name = url.strip_prefix(SCHEME)?;
    let valid = !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    valid.then(|| format!("{INPUT_PREFIX}{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_url_names_its_device() {
        assert_eq!(
            input_slot("disk://nyc-2024").as_deref(),
            Some("disk.in.nyc-2024")
        );
        assert_eq!(input_slot("disk://"), None);
        assert_eq!(input_slot("disk://Upper"), None);
        assert_eq!(input_slot("disk://a/b"), None);
        assert_eq!(input_slot("file:///data/x.parquet"), None);
        assert_eq!(input_slot(&format!("disk://{}", "a".repeat(25))), None);
        let longest = input_slot(&format!("disk://{}", "a".repeat(NAME_MAX))).expect("24 fits");
        assert_eq!(longest.len(), 32, "a slot name is at most 32 bytes");
    }

    #[test]
    fn a_serial_states_the_file_length() {
        assert_eq!(serial_of(123_456), "123456");
        assert_eq!(length_of(b"123456"), Some(123_456));
        let mut padded = [0u8; SERIAL_MAX];
        padded[..3].copy_from_slice(b"512");
        assert_eq!(length_of(&padded), Some(512));
        assert_eq!(length_of(b""), None);
        assert_eq!(length_of(&[0u8; SERIAL_MAX]), None);
        assert_eq!(length_of(b"12a"), None);
        assert_eq!(length_of(b"99999999999999999999"), None, "past u64");
        assert_eq!(serial_of(u64::MAX).len(), SERIAL_MAX);
    }
}
