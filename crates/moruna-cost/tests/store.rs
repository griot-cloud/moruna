//! SDD 24 k: the observation store (CM-T12 to CM-T14, CM-T20, CM-T21).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{batch, key, scratch, took};
use moruna_cost::{
    CostError, CostKey, LocalStore, ModelId, Observation, ObservationSource, ObservationStore,
    Origin, Retention, Target,
};

const MAGIC: [u8; 4] = [0xC0, 0x5D, 0xE7, 0x0B];

fn unit(k: &CostKey, n: u64) -> Observation {
    let mut o = took(Origin::Run, 1e-3 * (n + 1) as f64);
    o.set(Target::PeakMemory, 1000.0 + n as f64);
    Observation::new(k.clone(), batch(1000.0 + n as f64), o, n)
}

/// Every segment file under the store, oldest first by name within each key directory.
fn segments(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(root.join("observations")).expect("dir") {
        let dir = dir.expect("entry").path();
        for f in std::fs::read_dir(&dir).expect("key dir") {
            let f = f.expect("entry").path();
            if f.extension().is_some_and(|e| e == "seg") {
                out.push(f);
            }
        }
    }
    out.sort();
    out
}

fn records_on_disk(root: &Path) -> usize {
    segments(root)
        .iter()
        .map(|p| {
            std::fs::read(p)
                .expect("segment")
                .windows(4)
                .filter(|w| *w == MAGIC)
                .count()
        })
        .sum()
}

/// CM-T12 appends_survive_a_crash. CM-I6. A record cut short at any byte, or a tail of zeros
/// the file system extended the file with, is skipped and counted; the records before it are
/// read, and so are the records appended after it.
#[test]
fn cm_t12_appends_survive_a_crash() {
    let root = scratch("crash");
    let store = LocalStore::open(&root).expect("opens");
    let k = key("std", 1);
    for n in 0..6 {
        store.append(&unit(&k, n)).expect("appends");
    }
    let seg = segments(&root).pop().expect("a segment");
    let whole = std::fs::read(&seg).expect("read");
    let last = whole
        .windows(4)
        .rposition(|w| w == MAGIC)
        .expect("a record starts");
    for cut in [last + 1, last + 7, last + 13, whole.len() - 1] {
        std::fs::write(&seg, &whole[..cut]).expect("cut");
        let h = store.scan(&k).expect("scans");
        assert_eq!(h.observations.len(), 5, "cut at {cut}");
        assert_eq!(h.torn, 1, "cut at {cut}");
        assert_eq!(h.observations[4], unit(&k, 4));
    }
    // The process restarts and appends after the torn record: both are read, and the torn one
    // is still counted.
    let reopened = LocalStore::open(&root).expect("opens");
    reopened.append(&unit(&k, 6)).expect("appends");
    let h = reopened.scan(&k).expect("scans");
    let at: Vec<u64> = h.observations.iter().map(|o| o.at_ms).collect();
    assert_eq!(at, vec![0, 1, 2, 3, 4, 6]);
    assert_eq!(h.torn, 1);
    // A tail of zeros.
    let mut bytes = std::fs::read(&seg).expect("read");
    bytes.extend_from_slice(&[0u8; 100]);
    std::fs::write(&seg, &bytes).expect("zeros");
    let h = reopened.scan(&k).expect("scans");
    assert_eq!((h.observations.len(), h.torn), (6, 2));
    // A fit over the store reports what was skipped.
    let mut m = moruna_cost::LogLinear::new(moruna_cost::Defaults::standard());
    let report = moruna_cost::CostModel::fit(&mut m, &reopened).expect("fits");
    assert_eq!((report.keys, report.observations, report.torn), (1, 6, 2));
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T13 retention_holds. CM-I7. A key keeps its last `per_key` records raw and the rest in
/// its summary, the file system holds at most a retention and a segment, and a store opened
/// again sees the same.
#[test]
fn cm_t13_retention_holds() {
    let root = scratch("retention");
    let retention = Retention { per_key: 40 };
    let store = LocalStore::with_retention(&root, retention).expect("opens");
    assert_eq!(store.retention(), retention);
    assert_eq!(store.root(), root.as_path());
    let a = key("std", 1);
    let b = key("std", 2);
    for n in 0..200 {
        store.append(&unit(&a, n)).expect("appends");
        if n < 30 {
            store.append(&unit(&b, n)).expect("appends");
        }
        let h = store.scan(&a).expect("scans");
        assert!(h.observations.len() <= 40);
    }
    for reader in [
        &store,
        &LocalStore::with_retention(&root, retention).expect("reopens"),
    ] {
        let h = reader.scan(&a).expect("scans");
        let at: Vec<u64> = h.observations.iter().map(|o| o.at_ms).collect();
        assert_eq!(
            at,
            (160..200).collect::<Vec<u64>>(),
            "the last 40, oldest first"
        );
        let summary = h.summary.expect("older records summarised");
        assert_eq!(summary.count, 160);
        assert_eq!((summary.first_at_ms, summary.last_at_ms), (0, 159));
        assert_eq!(summary.targets[&Target::Duration].n, 160);
        assert_eq!(summary.rows.n, 160);
        let hb = reader.scan(&b).expect("scans");
        assert_eq!(hb.observations.len(), 30);
        assert!(hb.summary.is_none());
    }
    assert!(
        records_on_disk(&root) <= 40 + 10 + 30,
        "{} records on disk",
        records_on_disk(&root)
    );
    let mut all = Vec::new();
    store
        .scan_all(&mut |h| {
            all.push((h.key.clone(), h.observations.len()));
            Ok(())
        })
        .expect("scans all");
    all.sort();
    assert_eq!(all, vec![(a, 40), (b, 30)]);
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T14 fitted_state_beside_the_records. CM-I3. State is kept per model id, replaced by a
/// later save, and never returned for another id; a name that is not a name is refused.
#[test]
fn cm_t14_fitted_state_beside_the_records() {
    let root = scratch("state");
    let store = LocalStore::open(&root).expect("opens");
    let one = ModelId::new("loglinear", 1);
    let two = ModelId::new("loglinear", 2);
    assert_eq!(store.load_state(&one).expect("reads"), None);
    store.save_state(&one, b"first").expect("saves");
    store.save_state(&one, b"second").expect("saves");
    assert_eq!(
        store.load_state(&one).expect("reads"),
        Some(b"second".to_vec())
    );
    assert_eq!(store.load_state(&two).expect("reads"), None);
    assert!(root.join("models").join("loglinear@1.state").is_file());
    let bad = ModelId::new("../escape", 1);
    assert!(matches!(
        store.save_state(&bad, b"x"),
        Err(CostError::BadName { .. })
    ));
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T20 many_writers. CM-I6. Threads of one store and two stores on one directory (two
/// processes) append at once; every record is read back whole.
#[test]
fn cm_t20_many_writers() {
    let root = scratch("writers");
    let first = Arc::new(LocalStore::open(&root).expect("opens"));
    let second = Arc::new(LocalStore::open(&root).expect("opens"));
    let k = key("std", 1);
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let store = if t % 2 == 0 {
                Arc::clone(&first)
            } else {
                Arc::clone(&second)
            };
            let k = k.clone();
            std::thread::spawn(move || {
                for n in 0..100 {
                    store.append(&unit(&k, t * 1000 + n)).expect("appends");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("thread");
    }
    let h = first.scan(&k).expect("scans");
    assert_eq!((h.observations.len(), h.torn), (400, 0));
    std::fs::remove_dir_all(&root).ok();
}

/// CM-T21 refusals. CM-I6. The store keeps no observation it could not read back: inputs of
/// another version, a measured amount that is not one, a kind that is not a name; and a record
/// whose frame checks but whose body is not an observation is refused loudly, not skipped.
#[test]
fn cm_t21_refusals() {
    let root = scratch("refusals");
    let store = LocalStore::open(&root).expect("opens");
    let k = key("std", 1);
    let mut wrong_version = unit(&k, 0);
    wrong_version.inputs.version = 2;
    let mut negative = unit(&k, 0);
    negative.outcome.set(Target::SpillBytes, -1.0);
    let mut bad_kind = unit(&k, 0);
    bad_kind.key.kind = "Std".into();
    for o in [wrong_version, negative, bad_kind] {
        assert!(matches!(store.append(&o), Err(CostError::Invalid(_))));
    }
    assert!(store.scan(&k).expect("scans").observations.is_empty());

    store.append(&unit(&k, 0)).expect("appends");
    let seg = segments(&root).pop().expect("a segment");
    // A whole frame around a body that is not an observation: written the way the store frames.
    let body = br#"{"not":"an observation"}"#;
    let len = (body.len() as u32).to_le_bytes();
    let crc = crc32c(&[&len[..], &body[..]].concat());
    let mut frame = MAGIC.to_vec();
    frame.extend_from_slice(&len);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(body);
    let mut bytes = std::fs::read(&seg).expect("read");
    bytes.extend_from_slice(&frame);
    std::fs::write(&seg, &bytes).expect("write");
    let err = store.scan(&k).expect_err("refused");
    assert!(matches!(err, CostError::Unreadable { .. }), "{err}");
    assert!(err.to_string().contains("not an observation"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
        }
    }
    !c
}
