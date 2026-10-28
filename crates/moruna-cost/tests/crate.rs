//! SDD 24 k: the crate's shape and the cost of a prediction (CM-T1, CM-T2, CM-T18).

mod common;

use std::time::Instant;

use common::{Rng, batch, key, took};
use moruna_cost::{
    Capacity, Column, CostModel, Defaults, Inputs, Job, Kernel, Layout, LogLinear, Origin,
    Resources,
};

/// CM-T1 dependencies_are_the_four. CM-I1. The manifest names `moruna-job`, `serde`,
/// `serde_json` and `thiserror` and nothing else.
#[test]
fn cm_t1_dependencies_are_the_four() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("manifest");
    let mut names = Vec::new();
    let mut section = String::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        if section.contains("dependencies")
            && let Some((name, _)) = line.split_once('=')
            && !line.starts_with('#')
        {
            names.push((section.clone(), name.trim().to_string()));
        }
    }
    let expected: Vec<(String, String)> = ["moruna-job", "serde", "serde_json", "thiserror"]
        .iter()
        .map(|n| ("[dependencies]".to_string(), n.to_string()))
        .collect();
    assert_eq!(names, expected);
}

/// CM-T2 inputs_are_a_versioned_record. Every field of section 3 is written and read back; a
/// record that leaves them out reads as unknown.
#[test]
fn cm_t2_inputs_are_a_versioned_record() {
    let full = Inputs {
        version: moruna_cost::INPUTS_VERSION,
        data: moruna_cost::Data {
            rows: Some(10),
            bytes: Some(640),
            columns: vec![Column {
                name: "k".into(),
                bytes_mean: Some(8.0),
                bytes_p95: Some(12.0),
                null_fraction: Some(0.1),
                distinct: Some(4),
                key: true,
                skew: Some(0.3),
            }],
            columns_read: Some(1),
            layout: Some(Layout {
                codec: Some("zstd".into()),
                row_group_rows: Some(1 << 20),
                files: Some(3),
            }),
            selectivity: Some(0.5),
        },
        kernel: Kernel {
            deterministic: Some(true),
            preserves_columns: Some(false),
            stateful: Some(true),
            start_cold_seconds: Some(2.0),
            start_warm_seconds: Some(0.1),
            per_batch_seconds: Some(1e-3),
            per_row_seconds: Some(1e-7),
            per_byte_seconds: Some(1e-9),
            peak_per_byte: Some(2.0),
            retained_per_byte: Some(1.0),
            state_per_key_bytes: Some(64.0),
            checkpoint_bytes: Some(1e6),
            checkpoint_seconds: Some(0.2),
            worker_seconds_per_second: Some(1.0),
            speedup_2: Some(1.9),
            speedup_4: Some(3.5),
        },
        resources: Resources {
            workers: Some(2),
            given: Capacity {
                workers: Some(2),
                memory_bytes: Some(1 << 30),
                disk_bytes: Some(1 << 34),
                disk_bytes_per_second: Some(1 << 29),
            },
            ceiling: Capacity {
                workers: Some(8),
                ..Capacity::default()
            },
            free: Capacity {
                memory_bytes: Some(1 << 29),
                ..Capacity::default()
            },
            image_cached: Some(false),
        },
        job: Job {
            dependencies: Some(2),
            consumers: Some(3),
            contract_barrier: Some(true),
        },
    };
    let text = serde_json::to_string(&full).expect("serialises");
    let back: Inputs = serde_json::from_str(&text).expect("reads");
    assert_eq!(back, full);
    let sparse: Inputs = serde_json::from_str(r#"{"data": {"rows": 5}}"#).expect("reads");
    assert_eq!(sparse.version, moruna_cost::INPUTS_VERSION);
    assert_eq!(sparse.data.rows, Some(5));
    assert_eq!(sparse.kernel, Kernel::default());
}

/// CM-T18 predict_is_cheap. CM-I10. A warm key's prediction, in a model holding a hundred
/// keys, takes under 2 µs. The figure is reported everywhere and asserted in an optimised build
/// or on the reference host (preamble E1), since a debug build measures the compiler.
#[test]
fn cm_t18_predict_is_cheap() {
    const PREDICTS: u32 = 200_000;
    let mut rng = Rng::new(18);
    let mut m = LogLinear::new(Defaults::standard());
    for n in 0..100u8 {
        let k = key(if n % 2 == 0 { "std" } else { "package" }, n);
        for _ in 0..50 {
            let rows = rng.log_uniform(1e3, 1e6);
            m.observe(&k, &batch(rows), &took(Origin::Run, 1e-7 * rows));
        }
    }
    let k = key("std", 42);
    let inputs = batch(5e4);
    let mut sink = 0.0;
    for _ in 0..1_000 {
        sink += std::hint::black_box(&m)
            .predict(std::hint::black_box(&k), std::hint::black_box(&inputs))
            .duration
            .p50;
    }
    let start = Instant::now();
    for _ in 0..PREDICTS {
        sink += std::hint::black_box(&m)
            .predict(std::hint::black_box(&k), std::hint::black_box(&inputs))
            .duration
            .p50;
    }
    let per = start.elapsed().as_nanos() as f64 / f64::from(PREDICTS);
    assert!(sink > 0.0);
    let optimised = !cfg!(debug_assertions);
    println!(
        "cm_t18 predict: {per:.0} ns per call ({} build)",
        if optimised { "optimised" } else { "debug" }
    );
    if optimised || std::env::var_os("MORUNA_REFERENCE_HOST").is_some() {
        assert!(per < 2_000.0, "a warm predict took {per:.0} ns");
    }
}
