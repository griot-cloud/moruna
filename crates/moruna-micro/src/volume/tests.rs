//! Volumes against a driver in memory ([`super::fake`]).

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use moruna_image::layerfile::Entry;

use super::fake::{Fake, Heap, dir, file, layer, pattern, wait};
use super::io::{Bounce, Volume};
use super::layer::{Files, Run};

const MIB: u64 = 1 << 20;

/// Three files: 3 MiB and a bit of data with a page of zeros in it, a small one, and one in a
/// deeper directory.
fn tree() -> (Entry, Vec<u8>, Vec<u8>) {
    let mut big = pattern(3 * MIB as usize + 777, 7);
    big[MIB as usize..MIB as usize + 4096].fill(0);
    let small = b"vendor_id,fare\n1,12.5\n".to_vec();
    let root = dir(vec![
        (
            "nyc",
            dir(vec![
                ("2024-01.parquet", file(big.clone())),
                ("deeper", dir(vec![("note.txt", file(b"hello".to_vec()))])),
            ]),
        ),
        ("small.csv", file(small.clone())),
    ]);
    (root, big, small)
}

fn files(bytes: Vec<u8>, sector: u64, page_only: bool) -> Files {
    let fake = Fake::with(bytes, sector, (MIB / sector) as u32, true, page_only, None);
    Files::open(Volume::new("vol-a", fake, false).unwrap()).unwrap()
}

/// A destination of `len` bytes starting `skew` bytes past a page's start.
fn destination(len: usize, skew: usize) -> (Heap, u64) {
    let heap = Heap::new(len + skew);
    let addr = heap.addr() + skew as u64;
    (heap, addr)
}

fn read(files: &Files, path: &str, offset: u64, len: usize, skew: usize) -> Vec<u8> {
    let f = files.file(path).unwrap().unwrap();
    let (heap, addr) = destination(len, skew);
    wait(|done| files.read(&f, offset, addr, len as u64, done)).unwrap();
    heap.bytes()[skew..skew + len].to_vec()
}

#[test]
fn a_files_bytes_come_back_from_any_offset_with_only_the_edges_bounced() {
    let (root, big, small) = tree();
    let files = files(layer(&root, 0), 512, false);
    // The index's pages, read once a path is looked up, pass through a bounce; the files'
    // bytes do not.
    for path in ["nyc/2024-01.parquet", "small.csv", "nyc/deeper/note.txt"] {
        files.file(path).unwrap().unwrap();
    }
    let before = files.volume().stats();
    for (offset, len, skew) in [
        (0usize, big.len(), 0usize),
        (1, 4095, 3),
        (100, 2 * MIB as usize + 13, 17),
        (MIB as usize - 10, 5000, 1),
        (big.len() - 300, 300, 0),
    ] {
        assert_eq!(
            read(&files, "nyc/2024-01.parquet", offset as u64, len, skew),
            big[offset..offset + len],
            "{len} bytes at {offset}"
        );
    }
    assert_eq!(read(&files, "small.csv", 0, small.len(), 0), small);
    assert_eq!(read(&files, "nyc/deeper/note.txt", 1, 3, 0), b"ell");
    let s = files.volume().stats();
    let bounced = s.bounced_bytes - before.bounced_bytes;
    let placed = s.placed_bytes - before.placed_bytes;
    assert!(placed > 5 * MIB - 16 * 1024, "placed {placed}");
    // At most two edge sectors a run read: the bounce is the edges, never a body.
    assert!(bounced < 40 * 512, "bounced {bounced}");
    assert_eq!(s.fallbacks, 0);
}

#[test]
fn a_page_of_zeros_is_a_hole_read_as_zeros() {
    let (root, big, _) = tree();
    let files = files(layer(&root, 0), 512, false);
    let f = files.file("nyc/2024-01.parquet").unwrap().unwrap();
    assert_eq!(f.size, big.len() as u64);
    let hole = f.runs.iter().find(|r| r.volume.is_none()).copied();
    assert_eq!(
        hole,
        Some(Run {
            file: MIB,
            volume: None,
            len: 4096
        })
    );
    assert_eq!(f.runs.iter().map(|r| r.len).sum::<u64>(), f.size);
    let got = read(&files, "nyc/2024-01.parquet", MIB - 5, 4106, 0);
    assert_eq!(got, big[MIB as usize - 5..MIB as usize + 4101]);
}

#[test]
fn a_large_read_is_requests_of_the_largest_all_on_their_way_at_once() {
    let bytes = pattern(8 * MIB as usize, 3);
    let fake = Fake::new(bytes.clone(), 512, 2048, true);
    let volume = Volume::new("vol-b", fake.clone(), false).unwrap();
    let (heap, addr) = destination(4 * MIB as usize, 0);
    fake.close();
    let (tx, rx) = mpsc::channel();
    volume.read(MIB, addr, 4 * MIB, Box::new(move |r| tx.send(r).unwrap()));
    let s = volume.stats();
    assert_eq!(s.requests, 4, "four requests of 1 MiB");
    assert_eq!(
        s.in_flight_max, 4,
        "all four on their way before the first is answered"
    );
    fake.open();
    rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
    assert_eq!(
        heap.bytes()[..4 * MIB as usize],
        bytes[MIB as usize..5 * MIB as usize]
    );
    let s = volume.stats();
    assert_eq!((s.placed_bytes, s.bounced_bytes), (4 * MIB, 0));
}

#[test]
fn a_request_the_driver_cannot_place_moves_through_a_bounce_alone() {
    let (root, big, _) = tree();
    let files = files(layer(&root, 0), 512, true);
    // Placed at a page's start: no fallback.
    let n = 2 * MIB as usize;
    assert_eq!(read(&files, "nyc/2024-01.parquet", 0, n, 0), big[..n]);
    assert_eq!(files.volume().stats().fallbacks, 0);
    // Anywhere else: each placed request moves through a bounce instead, and the read is
    // whole.
    let before = files.volume().stats();
    assert_eq!(read(&files, "nyc/2024-01.parquet", 0, n, 8), big[..n]);
    let s = files.volume().stats();
    assert!(s.fallbacks - before.fallbacks >= 2, "{s:?}");
    assert!(s.bounced_bytes - before.bounced_bytes >= n as u64 - 8192);
}

#[test]
fn a_failed_request_fails_its_operation_and_no_other() {
    let bytes = pattern(4 * MIB as usize, 5);
    let fake = Fake::with(bytes.clone(), 512, 256, true, false, Some(4000));
    let volume = Volume::new("vol-c", fake, false).unwrap();
    let (_heap, addr) = destination(MIB as usize, 0);
    let why = wait(|done| volume.read(MIB, addr, MIB, done)).unwrap_err();
    assert!(
        why.contains("volume vol-c: reading at sector") && why.contains("Io"),
        "{why}"
    );
    let (heap, addr) = destination(1000, 0);
    wait(|done| volume.read(0, addr, 1000, done)).unwrap();
    assert_eq!(heap.bytes()[..1000], bytes[..1000]);
    // Past the end, refused before any request.
    let why = wait(|done| volume.read(4 * MIB - 1, addr, 2, done)).unwrap_err();
    assert!(why.contains("past volume vol-c"), "{why}");
}

#[test]
fn writes_are_whole_sectors_a_sinks_and_never_a_sources() {
    let fake = Fake::new(vec![0u8; 4 * MIB as usize], 4096, 256, false);
    let source = Volume::new("vol-d", fake.clone(), false).unwrap();
    let data = Heap::new(MIB as usize);
    let why = wait(|done| source.write(0, data.addr(), 4096, done)).unwrap_err();
    assert!(why.contains("read only"), "{why}");
    let ro = Fake::new(vec![0u8; MIB as usize], 512, 8, true);
    assert!(
        Volume::new("vol-e", ro, true)
            .unwrap_err()
            .contains("refuses every write")
    );
    let sink = Volume::new("vol-d", fake.clone(), true).unwrap();
    let mut data = Heap::new(2 * MIB as usize);
    let bytes = pattern(2 * MIB as usize, 9);
    data.bytes_mut().copy_from_slice(&bytes);
    wait(|done| sink.write(MIB, data.addr(), 2 * MIB, done)).unwrap();
    assert_eq!(sink.stats().requests, 2, "two requests of 1 MiB");
    wait(|done| sink.flush(done)).unwrap();
    assert_eq!(fake.flushes.load(Ordering::SeqCst), 1);
    assert_eq!(sink.read_bytes(MIB + 5, 10).unwrap(), bytes[5..15]);
    assert_eq!(
        fake.disk.lock().unwrap()[MIB as usize..3 * MIB as usize],
        bytes[..]
    );
    let why = wait(|done| sink.write(100, data.addr(), 4096, done)).unwrap_err();
    assert!(why.contains("not whole sectors"), "{why}");
    wait(|done| sink.write(0, data.addr(), 0, done)).unwrap();
}

#[test]
fn a_write_the_driver_cannot_place_moves_through_a_bounce_alone() {
    let fake = Fake::with(vec![0u8; 2 * MIB as usize], 512, 64, false, true, None);
    let sink = Volume::new("vol-f", fake.clone(), true).unwrap();
    let mut data = Heap::new(64 * 1024 + 4096);
    let bytes = pattern(64 * 1024, 11);
    data.bytes_mut()[4..4 + bytes.len()].copy_from_slice(&bytes);
    wait(|done| sink.write(4096, data.addr() + 4, bytes.len() as u64, done)).unwrap();
    let s = sink.stats();
    assert_eq!(
        s.fallbacks, 2,
        "two requests of 32 KiB, each placed elsewhere"
    );
    assert_eq!(s.bounced_bytes, bytes.len() as u64);
    assert_eq!(
        fake.disk.lock().unwrap()[4096..4096 + bytes.len()],
        bytes[..]
    );
}

#[test]
fn a_layers_files_are_listed_and_looked_up_by_path() {
    let (root, big, small) = tree();
    let files = files(layer(&root, 1 << 20), 512, false);
    let all = files.list("").unwrap();
    assert_eq!(
        all,
        [
            ("nyc/2024-01.parquet".to_string(), big.len() as u64),
            ("nyc/deeper/note.txt".to_string(), 5),
            ("small.csv".to_string(), small.len() as u64),
        ]
    );
    assert_eq!(files.list("nyc/").unwrap().len(), 2);
    assert_eq!(files.list("nyc/2024").unwrap().len(), 1);
    assert_eq!(files.list("sm").unwrap().len(), 1);
    assert!(files.list("nothing/").unwrap().is_empty());
    assert!(files.list("small.csv/").unwrap().is_empty());
    assert!(files.file("nyc").unwrap().is_none(), "a directory");
    assert!(files.file("nyc/none").unwrap().is_none());
    assert!(files.file("small.csv/x").unwrap().is_none());
    assert!(files.file("nyc/../small.csv").is_err());
    // Looked up again, the same file.
    let a = files.file("small.csv").unwrap().unwrap();
    let b = files.file("small.csv").unwrap().unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let f = files.file("small.csv").unwrap().unwrap();
    let (_h, addr) = destination(10, 0);
    let why = wait(|done| files.read(&f, 20, addr, 10, done)).unwrap_err();
    assert!(why.contains("past the file"), "{why}");
}

#[test]
fn a_layer_on_4_kib_sectors_reads_the_same() {
    let (root, big, _) = tree();
    let files = files(layer(&root, 0), 4096, false);
    assert_eq!(
        read(&files, "nyc/2024-01.parquet", 4095, 9000, 5),
        big[4095..13095]
    );
}

#[test]
fn a_volume_that_holds_no_layer_or_a_damaged_one_is_refused() {
    let fake = Fake::new(vec![0u8; MIB as usize], 512, 8, true);
    let why = Files::open(Volume::new("vol-g", fake, false).unwrap()).unwrap_err();
    assert!(why.contains("volume vol-g does not hold a layer"), "{why}");
    // A byte of the index changed: the page no longer matches its hash.
    let (root, _, _) = tree();
    let mut bytes = layer(&root, 0);
    bytes[4096 + 3] ^= 0x40;
    let fake = Fake::new(bytes, 512, 8, true);
    let files = Files::open(Volume::new("vol-h", fake, false).unwrap()).unwrap();
    let why = files.file("small.csv").unwrap_err();
    assert!(why.contains("does not match its hash"), "{why}");
}

#[test]
fn a_driver_that_says_nonsense_is_refused() {
    let mut fake = Fake::new(vec![0u8; 4096], 512, 8, true);
    Arc::get_mut(&mut fake).unwrap().geometry.max_sectors = 0;
    assert!(Volume::new("vol-i", fake, false).is_err());
}
