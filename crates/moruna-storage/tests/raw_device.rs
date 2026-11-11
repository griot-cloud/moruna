//! A real Linux block device through the strict raw adapter: geometry, exclusive writer
//! admission, aligned direct IO, refusals, and initial provisioning.
//!
//! Runs when `MORUNA_RAW_DEVICE` names an otherwise unused block device the test may
//! overwrite (a loop device over a scratch file); the quality gate sets one up on Linux.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::{Allocator, Tier};
use moruna_storage::Role;
use moruna_storage::raw::{Device, PathMode};
use moruna_storage_format::banks::Root;
use moruna_testkit::FakeAllocator;

fn device() -> Option<PathBuf> {
    std::env::var_os("MORUNA_RAW_DEVICE").map(PathBuf::from)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn raw_open_refuses_bad_roles_relative_paths_and_regular_files() {
    assert!(Device::open(Path::new("/dev/null"), Role::FormatProof, PathMode::Direct).is_err());
    assert!(Device::open(Path::new("dev/loop0"), Role::CacheScratch, PathMode::Direct).is_err());
    let file = std::env::temp_dir().join(format!("m-raw-regular-{}", std::process::id()));
    std::fs::write(&file, vec![0u8; 8192]).unwrap();
    let refused = Device::open(&file, Role::CacheScratch, PathMode::Direct);
    std::fs::remove_file(&file).unwrap();
    assert!(refused.is_err(), "a regular file is never a raw device");
}

#[test]
fn raw_device_provisions_then_serves_exclusive_aligned_direct_io() {
    let Some(path) = device() else {
        eprintln!("MORUNA_RAW_DEVICE unset: no block device to exercise");
        return;
    };
    // Zero the root cells so provisioning sees a fresh device.
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(&vec![0u8; 1 << 20], 0).unwrap();
        f.sync_all().unwrap();
    }
    let opened = Device::open(&path, Role::CacheScratch, PathMode::Direct).unwrap();
    let capacity = opened.capacity();
    let alignment = opened.alignment();
    assert!(capacity > 0 && alignment >= 4096 && alignment.is_power_of_two());
    assert_eq!(opened.role(), Role::CacheScratch);
    // A second exclusive open is refused while this one is held.
    assert!(Device::open(&path, Role::CacheScratch, PathMode::Direct).is_err());

    let root = Root::initial(
        capacity,
        alignment as u32,
        1 << 20,
        Role::CacheScratch,
        "workspace".into(),
        "bucket".into(),
    )
    .unwrap();
    // A shared handle cannot provision: provisioning needs unique ownership.
    let alias = Arc::clone(&opened);
    assert!(Arc::clone(&opened).provision(&root, None).is_err());
    drop(alias);
    opened.provision(&root, None).expect("initial provisioning");
    // Provisioning again over the now-written root is refused.
    let again = Device::open(&path, Role::CacheScratch, PathMode::Direct).unwrap();
    assert!(again.provision(&root, None).is_err());

    let device = Device::open(&path, Role::CacheScratch, PathMode::Direct).unwrap();
    let alloc = FakeAllocator::new();
    runtime().block_on(async {
        let writer = device.writer().unwrap();
        // One writer at a time.
        assert!(device.writer().is_err());
        let at = capacity - alignment as u64;
        let mut src = alloc.alloc(alignment, Tier::Host).unwrap();
        if src
            .host_ptr()
            .is_none_or(|p| !(p as usize).is_multiple_of(alignment))
        {
            eprintln!("allocator gave an unaligned page; skipping direct IO checks");
            return;
        }
        src.as_mut().fill(0x5a);
        writer.write(at, Arc::new(src).view()).await.unwrap();
        writer.sync().await.unwrap();
        let back = writer
            .read(at, alloc.alloc(alignment, Tier::Host).unwrap())
            .await
            .unwrap();
        assert!(back.as_ref().iter().all(|b| *b == 0x5a));
        drop(writer);
        // Admission is released once the writer and its operations are gone.
        let reader = device
            .read(at, alloc.alloc(alignment, Tier::Host).unwrap())
            .await
            .unwrap();
        assert_eq!(reader.as_ref()[0], 0x5a);
        // Misaligned offset and a range past the end are refused before IO.
        assert!(
            device
                .read(1, alloc.alloc(alignment, Tier::Host).unwrap())
                .await
                .is_err()
        );
        assert!(
            device
                .read(capacity, alloc.alloc(alignment, Tier::Host).unwrap())
                .await
                .is_err()
        );
        assert!(device.writer().is_ok());
    });
}
