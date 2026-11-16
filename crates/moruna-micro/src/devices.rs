//! The data devices' names: the contract between the launcher (stream D) and the engine
//! (stream C), frozen for the parallel streams (`architecture/data-os-streams.md`).
//!
//! The launcher attaches each input of a job as a read-only virtio-blk device and the run's
//! output disk as a writable one, and the engine's supervisor grants them to the engine under
//! these start-page names. A job names an input as `disk://NAME`, meaning the whole device
//! `disk.in.NAME` holds one file. The output disk holds, at the run's end, one layer file in
//! `moruna-image`'s format of the sinks' files, which the host reads with `moruna-layer`
//! (`architecture/arrow-core-0.7.md` decision 5, SDD 30 m.3).

/// The start-page name of input `NAME` is this prefix followed by `NAME`.
pub const INPUT_PREFIX: &str = "disk.in.";
/// The start-page name of the run's output disk.
pub const OUTPUT: &str = "disk.out";
/// The URL scheme a job uses for an input device.
pub const SCHEME: &str = "disk://";

/// The start-page name of the input a `disk://NAME` URL names, or `None` for another URL or
/// a name that is not one `[a-z0-9._-]{1,32}` word.
pub fn input_slot(url: &str) -> Option<String> {
    let name = url.strip_prefix(SCHEME)?;
    let valid = !name.is_empty()
        && name.len() <= 32
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
        assert_eq!(input_slot(&format!("disk://{}", "a".repeat(33))), None);
    }
}
