//! The report end to end, on a source tree, a kit and a job made for the test, with LLVM's
//! tools stood in for.

use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
};

use moruna_usage::{
    dump::{self, Header},
    elf::build::{Sec, elf},
};

use super::*;
use crate::llvm::{Counts, Export, File, Function, Segment, Summary, Tools};

const SYSCALL: &str = "pub fn dispatch(thread: &Thread) -> Flow {
    let result = match number {
        nr::ABI_VERSION => one(ABI_VERSION),
        nr::HANDLE_CLOSE => handle_close(p, a[0]),
        nr::MEMORY_PIN => memory_pin(p, a[0]),
        nr::SLOT_REFUSE => slot_refuse(p, a[0]),
        _ => Err(Error::BadSyscall.into()),
    };
}
";
const ABI: &str = "pub mod nr {
    pub const BASE: u64 = 0x1_0000;
    pub const ABI_VERSION: u64 = BASE;
    pub const HANDLE_CLOSE: u64 = BASE + 1;
    pub const MEMORY_PIN: u64 = BASE + 2;
    pub const SLOT_REFUSE: u64 = BASE + 3;
    pub const WAIT: u64 = BASE + 4;
}
";
const FOREIGN: &str = "fn fast_call(thread: &Thread, which: u8) -> Fast {
    match which {
        fast::BRK => Fast::Value(space.brk(a[0])),
        fast::EXIT => Fast::Exit(a[0] as i64),
        _ => Fast::Forward,
    }
}
";
const TABLE: &str = "pub mod nr {
    pub const READ: u64 = 0;
    pub const BRK: u64 = 12;
    pub const EXIT: u64 = 60;
    pub const RSEQ: u64 = 334;
}
";
const TASK: &str = "impl Task {
    fn call(&mut self, number: u64) -> Answer {
        match number {
            nr::READ => self.read(a[0]).into(),
            nr::RSEQ => self.rseq(a[0]),
            _ => self.refuse(number),
        }
    }
}
";
const RAW: &str = "pub fn wait(items: &mut [WaitItem]) -> u64 {
    unsafe { syscall(nr::WAIT, a) }.0
}
";

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("moruna-usage-report-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A program or kernel ELF with `counters` 8-byte counters at `addr`, and these functions.
fn binary(addr: u64, counters: usize, functions: &[(&str, u64)]) -> Vec<u8> {
    let cnts = vec![0u8; counters * 8];
    // The first function's name hash: the address, so each program's fingerprint differs.
    let mut data = [0u8; 64];
    data[..8].copy_from_slice(&addr.to_le_bytes());
    let symbols: Vec<(&str, u64, u64, u8)> = functions
        .iter()
        .enumerate()
        .map(|(i, (n, size))| (*n, 0x1000 + i as u64 * 0x100, *size, 2))
        .collect();
    elf(
        &[
            Sec {
                name: ".text",
                kind: 1,
                addr: 0x40_0000,
                bytes: &[0xc3; 32],
                size: 0,
            },
            Sec {
                name: "__llvm_prf_data",
                kind: 1,
                addr: addr + 0x10_0000,
                bytes: &data,
                size: 0,
            },
            Sec {
                name: "__llvm_prf_cnts",
                kind: 1,
                addr,
                bytes: &cnts,
                size: 0,
            },
            Sec {
                name: "__llvm_prf_names",
                kind: 1,
                addr: addr + 0x20_0000,
                bytes: b"n",
                size: 0,
            },
        ],
        &symbols,
        0,
    )
}

const STUB: &str = "core::core_arch::x86::avx512f::_mm512_set_epi8";

const KERNEL_FUNCTIONS: [(&str, u64); 7] = [
    ("moruna_microkernel::syscall::dispatch", 400),
    ("moruna_microkernel::syscall::handle_close", 40),
    ("moruna_microkernel::syscall::slot_refuse", 120),
    ("moruna_microkernel::syscall::memory_pin", 90),
    ("ostd::mm::vm_space::unused_reachable", 300),
    ("ostd::boot::unused_kernel_only", 500),
    ("ostd::mm::dma::map", 70),
];

fn segment(line: u64, col: u64, count: u64) -> Segment {
    Segment(line, col, count, true, true, false)
}

fn function(name: &str, file: &str, count: u64, first: u64, last: u64) -> Function {
    Function {
        name: name.into(),
        count,
        regions: vec![vec![first, 1, last, 2, count, 0, 0, 0]],
        filenames: vec![file.into()],
    }
}

fn file(name: &str, segments: Vec<Segment>, covered: u64, count: u64) -> File {
    File {
        filename: name.into(),
        segments,
        summary: Summary {
            lines: Counts { count, covered },
            ..Summary::default()
        },
    }
}

/// LLVM's tools stood in for: canned exports by binary, a disassembly, and a record of merges.
struct Fake {
    merges: RefCell<Vec<(usize, PathBuf)>>,
}

impl Tools for Fake {
    fn merge(&self, inputs: &[PathBuf], out: &Path) -> Result<(), String> {
        for i in inputs {
            assert!(i.is_file(), "{}", i.display());
        }
        self.merges
            .borrow_mut()
            .push((inputs.len(), out.to_path_buf()));
        fs::write(out, b"indexed").map_err(|e| e.to_string())
    }

    fn export(&self, binary: &Path, _profile: &Path) -> Result<Export, String> {
        let k = "/src/kernel/core/src/";
        let name = binary.file_name().unwrap().to_string_lossy();
        Ok(match name.as_ref() {
            "kernel.elf" | "moruna-microkernel.elf" => Export {
                files: vec![
                    file(
                        &format!("{k}syscall.rs"),
                        vec![
                            segment(1, 1, 9),
                            segment(3, 33, 2),
                            segment(4, 34, 5),
                            segment(6, 33, 0),
                        ],
                        6,
                        9,
                    ),
                    file(&format!("{k}foreign.rs"), vec![segment(3, 22, 4)], 2, 7),
                    file(
                        "/src/kernel/vendor/ostd-0.19.0/src/mm/vm_space.rs",
                        vec![],
                        10,
                        50,
                    ),
                ],
                functions: vec![
                    function(
                        "moruna_microkernel::syscall::dispatch",
                        &format!("{k}syscall.rs"),
                        9,
                        1,
                        9,
                    ),
                    function(
                        "moruna_microkernel::syscall::handle_close",
                        &format!("{k}syscall.rs"),
                        5,
                        20,
                        24,
                    ),
                    function(
                        "moruna_microkernel::syscall::slot_refuse",
                        &format!("{k}syscall.rs"),
                        0,
                        30,
                        41,
                    ),
                    function(
                        "moruna_microkernel::syscall::memory_pin",
                        &format!("{k}syscall.rs"),
                        0,
                        50,
                        59,
                    ),
                    function(
                        "ostd::mm::vm_space::unused_reachable",
                        "/src/kernel/vendor/ostd-0.19.0/src/mm/vm_space.rs",
                        0,
                        5,
                        24,
                    ),
                    function(
                        "ostd::boot::unused_kernel_only",
                        "/src/kernel/vendor/ostd-0.19.0/src/boot/mod.rs",
                        0,
                        1,
                        30,
                    ),
                    function(
                        "ostd::mm::dma::map",
                        "/src/kernel/vendor/ostd-0.19.0/src/mm/dma/mod.rs",
                        0,
                        1,
                        8,
                    ),
                    // Never instantiated: mapped, but not in the image.
                    function(
                        STUB,
                        "/rustlib/src/rust/library/core/src/x86/avx512f.rs",
                        0,
                        1,
                        900,
                    ),
                ],
                ..Export::default()
            },
            "linux" => Export {
                files: vec![
                    file(
                        "/src/kernel/linux/server/src/task.rs",
                        vec![segment(4, 25, 30), segment(5, 25, 0)],
                        3,
                        8,
                    ),
                    file("/src/kernel/sys/src/raw.rs", vec![], 2, 3),
                    file("/rustlib/src/rust/library/core/src/fmt.rs", vec![], 1, 100),
                ],
                functions: vec![
                    function(
                        "linux::task::Task::read",
                        "/src/kernel/linux/server/src/task.rs",
                        30,
                        10,
                        20,
                    ),
                    function(
                        "linux::task::Task::rseq",
                        "/src/kernel/linux/server/src/task.rs",
                        0,
                        21,
                        25,
                    ),
                    function(
                        "moruna_sys::raw::wait",
                        "/src/kernel/sys/src/raw.rs",
                        3,
                        1,
                        3,
                    ),
                ],
                ..Export::default()
            },
            _ => Export {
                files: vec![file("/src/kernel/net/server/src/main.rs", vec![], 0, 77)],
                ..Export::default()
            },
        })
    }

    fn functions(&self, profile: &Path) -> Result<std::collections::BTreeSet<String>, String> {
        let stem = profile.file_stem().unwrap().to_string_lossy();
        let binary = if stem == "kernel" {
            "kernel.elf"
        } else {
            stem.as_ref()
        };
        let export = self.export(Path::new(binary), profile)?;
        Ok(export
            .functions
            .into_iter()
            .map(|f| f.name)
            .filter(|n| n != STUB)
            .collect())
    }

    fn disassemble(&self, _binary: &Path) -> Result<String, String> {
        Ok("0000000000001000 <moruna_microkernel::process::run>:\n\
            1000:\tcallq\t0x1100 <moruna_microkernel::syscall::dispatch>\n\
            0000000000001100 <moruna_microkernel::syscall::dispatch>:\n\
            1100:\tcallq\t0x1400 <ostd::mm::vm_space::unused_reachable>\n\
            0000000000001400 <ostd::mm::vm_space::unused_reachable>:\n\
            1400:\tretq\n"
            .into())
    }
}

/// A source tree, a kit, and one job whose serial log has the Linux server's dump and line.
fn fixture(name: &str) -> (PathBuf, report::Config) {
    let root = scratch(name);
    let src = root.join("src");
    for (path, text) in [
        ("kernel/core/src/syscall.rs", SYSCALL),
        ("kernel/abi/src/lib.rs", ABI),
        ("kernel/core/src/foreign.rs", FOREIGN),
        ("kernel/linux/core/src/table.rs", TABLE),
        ("kernel/linux/server/src/task.rs", TASK),
        ("kernel/sys/src/raw.rs", RAW),
    ] {
        put(&src.join(path), text.as_bytes());
    }
    let kit = root.join("kit");
    let kernel = binary(0xffff_ffff_8830_0000, 6, &KERNEL_FUNCTIONS);
    put(&kit.join("moruna-microkernel.elf"), &kernel);
    put(
        &kit.join("node-user-bins/linux"),
        &binary(0x57_a010, 4, &[("linux::task::Task::rseq", 64)]),
    );
    put(
        &kit.join("node-user-bins/netstack"),
        &binary(0x60_0000, 2, &[]),
    );
    put(
        &kit.join("node-user-bins/storage"),
        &binary(0x62_0000, 2, &[]),
    );
    put(&kit.join("node-user-bins/gve"), &binary(0x63_0000, 2, &[]));
    put(
        &kit.join("node-user-bins/virtio-blk"),
        &binary(0x61_0000, 2, &[]),
    );
    let job = root.join("job");
    put(&job.join("kernel.elf"), &kernel);
    put(&job.join("kernel.cnts"), &[1u8; 48]);
    let mut log = String::from("kk: boot\n");
    let header = Header {
        program: "40.000000000057a010",
        process: "linux",
        instance: 7,
        seq: 0,
        addr: 0x57_a010,
        count: 4,
        wakes: 3,
    };
    let mut buf = [0u8; dump::LINE_MAX];
    dump::encode(&header, |i| i as u64, &mut buf, &mut |l: &[u8]| {
        log.push_str(std::str::from_utf8(l).unwrap())
    });
    log.push_str(
        "linux: moruna-engine (pid 3) exited with code 0; 40 calls served; refused: rseq(334) x2\n",
    );
    // The engine: position independent, so loaded elsewhere than its file says, and its lines
    // come through the Linux server's console with its prefix.
    let mut engine = binary(0x9_0000, 2, &[]);
    engine[16..18].copy_from_slice(&3u16.to_le_bytes());
    put(&root.join("image/moruna-engine"), &engine);
    let header = Header {
        program: "40.0000000000090000",
        process: "?",
        instance: 9,
        seq: 0,
        addr: 0x7f00_0009_0000,
        count: 2,
        wakes: 0,
    };
    let mut buf = [0u8; 300];
    dump::encode(&header, |i| i as u64 + 1, &mut buf, &mut |l: &[u8]| {
        log.push_str("linux moruna-engine: ");
        log.push_str(std::str::from_utf8(l).unwrap())
    });
    put(&job.join("serial.log"), log.as_bytes());
    let cfg = report::Config {
        src,
        kit,
        prod_kit: None,
        out: root.join("out"),
        runs: vec![("nyc-score".into(), job)],
        programs: vec![root.join("image/moruna-engine")],
    };
    (root, cfg)
}

#[test]
fn the_report_counts_calls_and_sorts_what_nothing_used() {
    let (root, cfg) = fixture("whole");
    let fake = Fake {
        merges: RefCell::new(Vec::new()),
    };
    let r = report::build(&cfg, &fake).unwrap();

    // Per job: the native arms, the Linux arms, the refusals, the programs, the areas.
    let job = &r.jobs[0];
    let native: Vec<_> = job.dispatch["native"]
        .iter()
        .map(|a| (a.names[0].as_str(), a.count))
        .collect();
    assert_eq!(
        native,
        [
            ("ABI_VERSION", Some(2)),
            ("HANDLE_CLOSE", Some(5)),
            ("MEMORY_PIN", Some(0)),
            ("SLOT_REFUSE", Some(0))
        ]
    );
    assert_eq!(job.dispatch["native"][1].numbers, [0x1_0001]);
    let server: Vec<_> = job.dispatch["linux-server"]
        .iter()
        .map(|a| a.count)
        .collect();
    assert_eq!(server, [Some(30), Some(0)]);
    let fast: Vec<_> = job.dispatch["linux-kernel"]
        .iter()
        .map(|a| (a.numbers.clone(), a.count))
        .collect();
    assert_eq!(fast, [(vec![12], Some(4)), (vec![60], Some(0))]);
    assert_eq!(job.refused[&334], 2);
    assert_eq!(job.linux_processes, [("moruna-engine".to_string(), 40)]);
    assert_eq!(job.programs["linux"].wakes, 3);
    assert_eq!(job.programs["linux"].lines, (5, 11));
    assert_eq!(job.programs["linux"].calls["WAIT"], 3);
    assert_eq!(
        job.kernel_areas["Moruna's core (kernel/core)"],
        [8, 16, 2, 4]
    );
    assert_eq!(job.kernel_areas["OSTD 0.19"], [10, 50, 0, 3]);
    assert!(
        !job.kernel_areas.contains_key("core and alloc"),
        "the stub is not in the image"
    );
    assert_eq!((job.dumps, job.broken_dumps), (2, 0));
    // The engine, found by --program and loaded elsewhere than its file says.
    assert_eq!(job.programs["moruna-engine"].lines, (0, 77));
    assert!(
        cfg.out
            .join("jobs/nyc-score/moruna-engine.profraw")
            .is_file()
    );
    assert_eq!(r.all.programs["linux"].wakes, 3);

    // What nothing used, in the coordinator's order, and the accelerator path apart.
    let groups: Vec<_> = r
        .unused
        .iter()
        .map(|i| (i.group.as_str(), i.name.as_str()))
        .collect();
    assert_eq!(
        groups,
        [
            ("A", "SLOT_REFUSE"),
            ("B", "ostd-0.19.0/src/mm/vm_space.rs"),
            ("C", "RSEQ"),
            ("C", "EXIT"),
            ("D", "storage"),
            ("E", "ostd-0.19.0/src/boot/mod.rs"),
        ]
    );
    let slot = &r.unused[0];
    assert_eq!((slot.lines, slot.bytes), (12, 120));
    assert!(
        slot.detail.contains("handler slot_refuse"),
        "{}",
        slot.detail
    );
    assert_eq!((r.unused[1].lines, r.unused[1].bytes), (20, 300));
    assert!(
        r.unused[2].detail.contains("refused 2 times"),
        "{}",
        r.unused[2].detail
    );
    assert_eq!((r.unused[2].lines, r.unused[2].bytes), (5, 64));
    assert_eq!(r.unused[4].detail, "never started");
    assert_eq!(r.unused[4].lines, 77);
    let keep: Vec<_> = r
        .keep
        .iter()
        .map(|i| (i.group.as_str(), i.name.as_str()))
        .collect();
    assert!(keep.contains(&("native call", "MEMORY_PIN")), "{keep:?}");
    assert!(keep.contains(&("program", "virtio-blk")), "{keep:?}");
    assert!(
        keep.contains(&("kernel-mode code", "ostd-0.19.0/src/mm/dma/mod.rs")),
        "{keep:?}"
    );
    assert!(r.notes.iter().any(|n| n.contains("no --prod-kit")));

    // The profiles: one per binary per job, then merged over the jobs.
    assert!(cfg.out.join("jobs/nyc-score/kernel.profraw").is_file());
    assert!(cfg.out.join("jobs/nyc-score/linux.profraw").is_file());
    assert!(cfg.out.join("all/kernel.profdata").is_file());
    assert!(cfg.out.join("zero/netstack.profraw").is_file());
    // Mode 1 keeps the net server whatever the job VMs use, apart from the removals.
    let mode1: Vec<_> = r
        .keep_mode1
        .iter()
        .map(|i| (i.name.as_str(), i.detail.as_str(), i.lines))
        .collect();
    assert_eq!(
        mode1,
        [
            (
                "gve",
                "never started; kept for the accelerator path too",
                77
            ),
            ("netstack", "never started", 77)
        ]
    );
    assert!(r.keep.iter().any(|i| i.name == "gve"));

    // And the command writes both files.
    let said = run(
        Command::Report {
            src: cfg.src.clone(),
            kit: cfg.kit.clone(),
            prod_kit: Some(cfg.kit.clone()),
            llvm: None,
            out: cfg.out.clone(),
            runs: cfg.runs.clone(),
            programs: cfg.programs.clone(),
        },
        Some(&fake),
    )
    .unwrap();
    assert!(said.contains("1 jobs, 6 candidates"), "{said}");
    let md = fs::read_to_string(cfg.out.join("usage.md")).unwrap();
    assert!(md.contains("| rseq | 334 | 2 | 2 |"), "{md}");
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(cfg.out.join("usage.json")).unwrap()).unwrap();
    assert_eq!(json["unused"][0]["name"], "SLOT_REFUSE");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn evidence_from_another_build_or_tree_is_refused() {
    let (root, mut cfg) = fixture("refuse");
    let fake = Fake {
        merges: RefCell::new(Vec::new()),
    };
    // The kit's Linux server is another build: no program in the kit has the dumps'
    // fingerprint.
    put(
        &cfg.kit.join("node-user-bins/linux"),
        &binary(0x57_b000, 4, &[]),
    );
    let err = report::build(&cfg, &fake).unwrap_err();
    assert!(err.contains("not in the kit"), "{err}");
    // Its fingerprint, but another counter layout.
    let mut data = [0u8; 64];
    data[..8].copy_from_slice(&0x57_a010u64.to_le_bytes());
    let other = elf(
        &[
            Sec {
                name: "__llvm_prf_data",
                kind: 1,
                addr: 0x70_0000,
                bytes: &data,
                size: 0,
            },
            Sec {
                name: "__llvm_prf_cnts",
                kind: 1,
                addr: 0x57_b000,
                bytes: &[0; 32],
                size: 0,
            },
            Sec {
                name: "__llvm_prf_names",
                kind: 1,
                addr: 0x80_0000,
                bytes: b"n",
                size: 0,
            },
        ],
        &[],
        0,
    );
    put(&cfg.kit.join("node-user-bins/linux"), &other);
    let err = report::build(&cfg, &fake).unwrap_err();
    assert!(err.contains("not of the kit's build"), "{err}");
    // Two programs of one build cannot be told apart.
    put(&cfg.kit.join("node-user-bins/linux-copy"), &other);
    let err = report::build(&cfg, &fake).unwrap_err();
    assert!(err.contains("each of linux|linux-copy"), "{err}");
    fs::remove_file(cfg.kit.join("node-user-bins/linux-copy")).unwrap();
    // The kernel's counters are not the kernel's size.
    put(&cfg.runs[0].1.join("kernel.cnts"), &[0u8; 8]);
    assert!(report::build(&cfg, &fake).unwrap_err().contains("kernel"));
    cfg.src = root.join("nowhere");
    assert!(
        report::build(&cfg, &fake)
            .unwrap_err()
            .contains("not the usage build's source tree")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_raw_profile_is_built_from_an_elf_and_its_counters() {
    let root = scratch("profraw");
    put(&root.join("k.elf"), &binary(0x8000, 2, &[]));
    put(&root.join("k.cnts"), &[0u8; 16]);
    let said = run(
        Command::Profraw {
            elf: root.join("k.elf"),
            counters: root.join("k.cnts"),
            out: root.join("k.profraw"),
        },
        None,
    )
    .unwrap();
    assert!(said.starts_with("wrote"));
    let raw = fs::read(root.join("k.profraw")).unwrap();
    assert_eq!(
        u64::from_le_bytes(raw[..8].try_into().unwrap()),
        moruna_usage::profraw::MAGIC
    );
    let short = run(
        Command::Profraw {
            elf: root.join("k.elf"),
            counters: root.join("k.elf"),
            out: root.join("x"),
        },
        None,
    );
    assert!(short.unwrap_err().contains("counters"));
    assert!(
        run(
            Command::Profraw {
                elf: root.join("none"),
                counters: root.join("k.cnts"),
                out: root.join("x")
            },
            None
        )
        .is_err()
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_command_line_is_parsed_or_refused_by_name() {
    let args = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(
        parse(&args("report --src s --kit k --out o a=d1 b=d2")).unwrap(),
        Command::Report {
            src: "s".into(),
            kit: "k".into(),
            prod_kit: None,
            llvm: None,
            out: "o".into(),
            runs: vec![("a".into(), "d1".into()), ("b".into(), "d2".into())],
            programs: vec![],
        }
    );
    let with = parse(&args(
        "report --src s --kit k --program e1 --program e2 --out o a=d",
    ))
    .unwrap();
    assert!(
        matches!(with, Command::Report { programs, .. } if programs == [PathBuf::from("e1"), PathBuf::from("e2")])
    );
    assert_eq!(
        parse(&args("profraw --elf e --counters c --out o")).unwrap(),
        Command::Profraw {
            elf: "e".into(),
            counters: "c".into(),
            out: "o".into()
        }
    );
    for (bad, why) in [
        ("report --src s --kit k --out o", "no jobs"),
        ("report --src s --kit k a=d", "--out is needed"),
        ("report --src s --src t --kit k --out o a=d", "given twice"),
        (
            "report --src s --kit k --out o a=d a=e",
            "job a given twice",
        ),
        ("report --src s --kit k --out o nolabel", "LABEL=DIR"),
        (
            "report --src s --kit k --out o --what x a=d",
            "--what is not an option",
        ),
        ("profraw --elf e --counters c --out o a=d", "takes no jobs"),
        (
            "profraw --elf e --counters c --out o --program p",
            "takes no jobs or programs",
        ),
        ("profraw --elf", "needs a value"),
        ("explain", "unknown command"),
    ] {
        let err = parse(&args(bad)).unwrap_err();
        assert!(err.contains(why), "{bad}: {err}");
    }
    assert!(parse(&[]).unwrap_err().starts_with("usage:"));
}

#[test]
fn sizes_areas_and_the_accelerator_path_are_named() {
    assert_eq!(
        report::short("/data/src/kernel/vendor/ostd-0.19.0/src/mm/dma/mod.rs"),
        "ostd-0.19.0/src/mm/dma/mod.rs"
    );
    assert_eq!(
        report::short("/home/x/.cargo/registry/src/index.crates.io-1/spin-0.9/src/lib.rs"),
        "spin-0.9/src/lib.rs"
    );
    assert_eq!(
        report::short("/data/src/kernel/core/src/vm.rs"),
        "kernel/core/src/vm.rs"
    );
    assert_eq!(
        report::short("/rustc/library/core/src/fmt.rs"),
        "core/src/fmt.rs"
    );
    assert_eq!(report::short("other.rs"), "other.rs");
    assert_eq!(
        report::area("/x/kernel/boot/src/main.rs"),
        "boot (kernel/boot)"
    );
    assert_eq!(
        report::area("/x/kernel/vendor/osdk-frame-allocator-0.19.0/src/lib.rs"),
        "frame allocator (OSDK)"
    );
    assert_eq!(
        report::area("/x/kernel/abi/src/lib.rs"),
        "kernel crates (abi, loader, handles, quota, datapath)"
    );
    assert_eq!(report::area("/x/spin/src/lib.rs"), "dependencies");
    assert!(report::keep(
        "moruna_microkernel::syscall::interrupt_create_msi",
        "/x/kernel/core/src/syscall.rs"
    ));
    assert!(report::keep(
        "ostd::arch::iommu::x",
        "/x/kernel/vendor/ostd-0.19.0/src/arch/x86/iommu/mod.rs"
    ));
    assert!(!report::keep(
        "moruna_microkernel::vm::Space::fault",
        "/x/kernel/core/src/vm.rs"
    ));
    // A type argument that names DMA does not make a library function DMA's.
    assert!(!report::keep(
        "<alloc::Node<ostd::mm::dma::X>>::len",
        "/rust/library/alloc/src/x.rs"
    ));
    assert!(!report::keep(
        "moruna_microkernel::object::get::<moruna_microkernel::memory::Pin>",
        "/x/kernel/core/src/object.rs"
    ));
    assert!(report::keep(
        "<moruna_microkernel::memory::DeviceMemory>::new",
        "/x/kernel/core/src/memory.rs"
    ));
    let elf = binary(0x9000, 1, &[("f", 10), ("f", 6), ("g", 0)]);
    assert_eq!(report::sizes(&elf).unwrap()["f"], 16);
    assert!(report::loaded_bytes(&elf) > 0);
    assert_eq!(report::loaded_bytes(b"no"), 0);
    assert!(report::sizes(b"no").is_err());
}
