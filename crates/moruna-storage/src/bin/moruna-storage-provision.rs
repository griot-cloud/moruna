//! Explicit operator tool, never invoked by ordinary storage mount/startup.
use moruna_storage::provision;
use moruna_storage_format::{
    Role,
    banks::Root,
    config::{BootConfig, MAX_CONFIG},
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
const USAGE: &str = "usage:\n  moruna-storage-provision plan --output ABS_ROOT --capacity BYTES --alignment BYTES --log-bytes BYTES --role checkpoint-metadata|cache-scratch|single-node-home --workspace OWNER --bucket BUCKET\n  moruna-storage-provision format --image ABS_EXISTING_IMAGE --root ABS_ROOT [--layout ABS_BOOT_CONFIG]\n  moruna-storage-provision format --raw ABS_BLOCK_DEVICE --root ABS_ROOT [--layout ABS_BOOT_CONFIG] (Linux only)";
fn options(args: impl Iterator<Item = String>) -> Result<BTreeMap<String, String>, String> {
    let mut args = args;
    let mut fields = BTreeMap::new();
    while let Some(name) = args.next() {
        if !name.starts_with("--") {
            return Err(format!("expected explicit option: {name}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {name}"))?;
        if value.starts_with("--") || fields.insert(name.clone(), value).is_some() {
            return Err(format!("missing or duplicate option: {name}"));
        }
    }
    Ok(fields)
}
fn take(fields: &mut BTreeMap<String, String>, name: &str) -> Result<String, String> {
    fields
        .remove(name)
        .ok_or_else(|| format!("required option missing: {name}"))
}
fn number(fields: &mut BTreeMap<String, String>, name: &str) -> Result<u64, String> {
    let text = take(fields, name)?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid unsigned integer: {name}"));
    }
    text.parse()
        .map_err(|_| format!("integer out of range: {name}"))
}
fn path(value: String) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("explicit absolute path required".into());
    }
    Ok(path)
}
fn leftovers(fields: BTreeMap<String, String>) -> Result<(), String> {
    if let Some((name, _)) = fields.into_iter().next() {
        Err(format!("unknown option: {name}"))
    } else {
        Ok(())
    }
}
fn read_root(path: &Path) -> Result<Root, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("root artifact open: {error}"))?;
    file.try_lock_shared()
        .map_err(|error| format!("root artifact lock: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("root artifact metadata: {error}"))?;
    if !metadata.is_file() || metadata.len() > provision::MAX_ALIGNMENT as u64 {
        return Err("root artifact must be a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(provision::MAX_ALIGNMENT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("root artifact read: {error}"))?;
    if bytes.len() > provision::MAX_ALIGNMENT {
        return Err("root artifact grew beyond bound".into());
    }
    let root = Root::decode(&bytes).map_err(|error| format!("root artifact decode: {error}"))?;
    provision::validate_root(&root).map_err(|error| error.to_string())?;
    Ok(root)
}
fn read_layout(path: &Path) -> Result<BootConfig, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("layout open: {error}"))?;
    file.try_lock_shared()
        .map_err(|error| format!("layout lock: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("layout metadata: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG as u64 {
        return Err("layout must be a bounded canonical boot configuration file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("layout read: {error}"))?;
    if bytes.len() > MAX_CONFIG {
        return Err("layout grew beyond bound".into());
    }
    (if bytes.len() == MAX_CONFIG {
        BootConfig::decode_page(&bytes)
    } else {
        BootConfig::decode(&bytes)
    })
    .map_err(|error| format!("layout decode: {error}"))
}
fn execute(command: &str, mut fields: BTreeMap<String, String>) -> Result<(), String> {
    match command {
        "plan" => {
            let output = path(take(&mut fields, "--output")?)?;
            let capacity = number(&mut fields, "--capacity")?;
            let alignment = u32::try_from(number(&mut fields, "--alignment")?)
                .map_err(|_| "alignment width".to_string())?;
            let log_bytes = number(&mut fields, "--log-bytes")?;
            let role = match take(&mut fields, "--role")?.as_str() {
                "checkpoint-metadata" => Role::CheckpointMetadata,
                "cache-scratch" => Role::CacheScratch,
                "single-node-home" => Role::SingleNodeHome,
                _ => return Err("explicit supported role required".into()),
            };
            let workspace = take(&mut fields, "--workspace")?;
            let bucket = take(&mut fields, "--bucket")?;
            leftovers(fields)?;
            let root = Root::initial(capacity, alignment, log_bytes, role, workspace, bucket)
                .map_err(|error| format!("root plan: {error}"))?;
            provision::validate_root(&root).map_err(|error| error.to_string())?;
            let encoded = root
                .encode()
                .map_err(|error| format!("root encode: {error}"))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&output)
                .map_err(|error| format!("new root artifact: {error}"))?;
            file.write_all(&encoded)
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("root artifact may exist; write/sync failed: {error}"))?;
            let parent = output.parent().ok_or("absolute artifact parent missing")?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("root artifact exists; directory sync failed: {error}"))?;
            println!(
                "planned disk-v3 {:?}; physical={} payload_per_bank={}",
                root.geometry().role,
                root.physical_capacity(),
                root.payload_capacity()
            );
            Ok(())
        }
        "format" => {
            let root_path = path(take(&mut fields, "--root")?)?;
            let image = fields.remove("--image");
            let raw = fields.remove("--raw");
            let layout = fields
                .remove("--layout")
                .map(path)
                .transpose()?
                .map(|path| read_layout(&path))
                .transpose()?;
            leftovers(fields)?;
            let root = read_root(&root_path)?;
            let report = match (image, raw) {
                (Some(image), None) => provision::image(&path(image)?, &root, layout.as_ref())
                    .map_err(|error| error.to_string())?,
                (None, Some(raw)) => {
                    #[cfg(target_os = "linux")]
                    {
                        provision::raw(&path(raw)?, &root, layout.as_ref())
                            .map_err(|error| error.to_string())?
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = raw;
                        return Err(
                            "raw block provisioning requires Linux; no file fallback".into()
                        );
                    }
                }
                _ => return Err("select exactly one explicit --image or --raw target".into()),
            };
            println!(
                "synced and verified disk-v3 {:?}; raw_device={} physical={} payload_per_bank={}",
                report.root.geometry().role,
                report.raw_device,
                report.root.physical_capacity(),
                report.root.payload_capacity()
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}
fn main() {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        std::process::exit(2)
    };
    if command == "--help" {
        println!("{USAGE}");
        return;
    }
    if let Err(error) = options(args).and_then(|fields| execute(&command, fields)) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }
    fn fields(list: &[&str]) -> BTreeMap<String, String> {
        options(args(list)).unwrap()
    }
    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir() -> Dir {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("m-prov-{}-{name}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Dir(path.canonicalize().unwrap())
    }

    #[test]
    fn options_refuse_positional_missing_and_duplicate_values() {
        assert!(options(args(&["plan"])).is_err());
        assert!(options(args(&["--output"])).is_err());
        assert!(options(args(&["--a", "--b"])).is_err());
        assert!(options(args(&["--a", "1", "--a", "2"])).is_err());
        assert_eq!(fields(&["--a", "1"]).get("--a").unwrap(), "1");
    }

    #[test]
    fn numbers_paths_and_leftovers_are_strict() {
        assert!(number(&mut fields(&["--n", "12x"]), "--n").is_err());
        assert!(number(&mut fields(&["--n", "99999999999999999999999"]), "--n").is_err());
        assert!(number(&mut BTreeMap::new(), "--n").is_err());
        assert_eq!(number(&mut fields(&["--n", "42"]), "--n").unwrap(), 42);
        assert!(path("relative".into()).is_err());
        assert!(leftovers(fields(&["--extra", "1"])).is_err());
        assert!(leftovers(BTreeMap::new()).is_ok());
    }

    #[test]
    fn execute_refuses_unknown_commands_roles_and_targets() {
        assert_eq!(execute("nope", BTreeMap::new()).unwrap_err(), USAGE);
        let bad_role = fields(&[
            "--output",
            "/tmp/x",
            "--capacity",
            "1",
            "--alignment",
            "1",
            "--log-bytes",
            "1",
            "--role",
            "wrong",
            "--workspace",
            "w",
            "--bucket",
            "b",
        ]);
        assert!(execute("plan", bad_role).unwrap_err().contains("role"));
        let wide = fields(&[
            "--output",
            "/tmp/x",
            "--capacity",
            "1",
            "--alignment",
            "4294967296",
            "--log-bytes",
            "1",
        ]);
        assert!(execute("plan", wide).unwrap_err().contains("alignment"));
        assert!(execute("format", fields(&["--root", "/nonexistent/root"])).is_err());
    }

    #[test]
    fn plan_writes_once_and_format_reads_it_back_with_strict_target_selection() {
        let scratch = dir();
        let root = scratch.0.join("root.bin");
        let plan = |output: &Path| {
            fields(&[
                "--output",
                output.to_str().unwrap(),
                "--capacity",
                "67108864",
                "--alignment",
                "4096",
                "--log-bytes",
                "1048576",
                "--role",
                "checkpoint-metadata",
                "--workspace",
                "workspace",
                "--bucket",
                "bucket",
            ])
        };
        match execute("plan", plan(&root)) {
            Ok(()) => {
                // Never overwrites an existing artifact.
                assert!(execute("plan", plan(&root)).is_err());
                assert!(read_root(&root).is_ok());
                // Format needs exactly one target.
                let none = fields(&["--root", root.to_str().unwrap()]);
                assert!(execute("format", none).unwrap_err().contains("exactly one"));
                let image = scratch.0.join("missing.img");
                let missing = fields(&[
                    "--root",
                    root.to_str().unwrap(),
                    "--image",
                    image.to_str().unwrap(),
                ]);
                assert!(execute("format", missing).is_err());
            }
            // A geometry the format refuses is still a typed refusal, not a panic.
            Err(error) => assert!(error.contains("root")),
        }
        // Layout and root readers refuse non-files and garbage.
        assert!(read_layout(&scratch.0).is_err());
        let junk = scratch.0.join("junk");
        std::fs::write(&junk, b"not a layout").unwrap();
        assert!(read_layout(&junk).is_err());
        assert!(read_root(&junk).is_err());
        assert!(read_root(&scratch.0).is_err());
    }
}
