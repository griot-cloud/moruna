//! SV-T9 (SDD 23, k): the boot image tool parses a description, refuses a bad one by
//! line, renders the manifest the kernel parses, checks every program is loadable,
//! and writes an initramfs the kernel's reader reads back.

use std::{fs, path::PathBuf, process::Command};

use moruna_abi::{Rights, manifest::Manifest};
use moruna_image::{DescSource, Description, archive, build};
use moruna_loader::cpio;

/// A static ELF64 image the kernel's loader accepts: one read-and-execute segment.
fn program() -> Vec<u8> {
    let mut b = vec![0u8; 0x1000 + 16];
    b[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    b[4] = 2;
    b[5] = 1;
    b[6] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&62u16.to_le_bytes());
    b[24..32].copy_from_slice(&0x40_1000u64.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    let ph = 64;
    b[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
    b[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
    b[ph + 8..ph + 16].copy_from_slice(&0x1000u64.to_le_bytes());
    b[ph + 16..ph + 24].copy_from_slice(&0x40_1000u64.to_le_bytes());
    b[ph + 32..ph + 40].copy_from_slice(&16u64.to_le_bytes());
    b[ph + 40..ph + 48].copy_from_slice(&16u64.to_le_bytes());
    b
}

const DESCRIPTION: &str = "\
# a test image
moruna-image 1
expect kk: 1 CPUs online

process supervisor
  handle console console
  handle root.process root.process
  handle program:child child-image
  expect supervisor: ok
  exits 0

process ping
  handle console console rights=write,transfer
  exits 0
process pong pong-program
  faults page fault at 0x0 on read

link ping.peer pong.peer:read,write,wait
link supervisor.a-very-long-slot-name-indeed pong.back
";

fn programs(names: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-image-test-{}-{}",
        std::process::id(),
        names.join("-")
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    for name in names {
        fs::write(dir.join(name), program()).expect("program");
    }
    dir
}

#[test]
fn sv_t9_a_description_renders_the_manifest_the_kernel_parses() {
    let d = Description::parse(DESCRIPTION).expect("parses");
    assert_eq!(d.processes.len(), 3);
    assert_eq!(d.processes[2].program, "pong-program");
    assert_eq!(
        d.processes[0].handles[2].source,
        DescSource::Program("child".into())
    );
    assert_eq!(
        d.processes[1].handles[0].rights,
        Some(Rights::WRITE | Rights::TRANSFER)
    );
    assert_eq!(
        d.links[0].ends[1].rights,
        Some(Rights::READ | Rights::WRITE | Rights::WAIT)
    );
    assert_eq!(
        d.programs().into_iter().collect::<Vec<_>>(),
        ["child", "ping", "pong-program", "supervisor"]
    );
    let (manifest, text) = d.manifest().expect("manifest");
    assert_eq!(Manifest::parse(text.as_bytes()), Ok(manifest.clone()));
    assert_eq!(manifest.channels, ["ping.peer", "link1"]);
    assert!(text.contains("handle file:bin/child child-image"));
    assert!(text.contains("handle channel:ping.peer.1 peer rights=read,write,wait"));
    let expect = d.expectations();
    assert_eq!(expect[0], "kk: 1 CPUs online");
    assert!(expect.contains(
        &"kk: moruna microkernel, abi 1.1: started 3 processes from the manifest".to_string()
    ));
    assert!(expect.contains(&"kk: process supervisor exited with code 0 (".to_string()));
    assert!(expect.contains(&"kk: process pong faulted: page fault at 0x0 on read".to_string()));
    assert!(expect.contains(&"supervisor: ok".to_string()));
    assert_eq!(
        expect.last().map(String::as_str),
        Some("kk: every process has ended; the supervisor succeeded; powering off")
    );
}

#[test]
fn sv_t9_a_bad_description_is_refused_by_line() {
    let cases: &[(&str, usize, &str)] = &[
        ("", 0, "empty"),
        ("moruna-image 2\n", 1, "first line"),
        ("moruna-image 1\nprocess\n", 2, "process NAME"),
        ("moruna-image 1\nprocess Bad\n", 2, "not 1 to 32 bytes"),
        ("moruna-image 1\nprocess a\nprocess a\n", 3, "twice"),
        ("moruna-image 1\nhandle console c\n", 2, "must follow"),
        (
            "moruna-image 1\nprocess a\nhandle disk d\n",
            3,
            "not a handle source",
        ),
        (
            "moruna-image 1\nprocess a\nhandle console\n",
            3,
            "handle SOURCE",
        ),
        (
            "moruna-image 1\nprocess a\nhandle console c read\n",
            3,
            "rights=",
        ),
        (
            "moruna-image 1\nprocess a\nhandle console c rights=fly\n",
            3,
            "not a right",
        ),
        ("moruna-image 1\nprocess a\nlink a.x\n", 3, "link PROCESS"),
        ("moruna-image 1\nprocess a\nlink a b.y\n", 3, "PROCESS.SLOT"),
        ("moruna-image 1\nprocess a\nexits zero\n", 3, "exit code"),
        ("moruna-image 1\nexits 0\n", 2, "must follow"),
        ("moruna-image 1\nexpect\n", 2, "needs its text"),
        ("moruna-image 1\nfrobnicate\n", 2, "not a directive"),
    ];
    for (text, line, words) in cases {
        let e = Description::parse(text).expect_err(text);
        assert_eq!(e.line, *line, "{text:?}: {e}");
        assert!(e.reason.contains(words), "{text:?}: {e}");
    }
    let d = Description::parse("moruna-image 1\nprocess a\nlink a.x b.y\n").expect("parses");
    assert!(
        d.manifest()
            .expect_err("no b")
            .to_string()
            .starts_with("line 3: no process `b`")
    );
    let d = Description::parse("moruna-image 1\nprocess a\nprocess b\nhandle root.process r\n")
        .expect("parses");
    let e = d.manifest().expect_err("root outside the supervisor");
    assert_eq!(e.line, 0);
    assert!(e.reason.contains("handle root.process r"), "{e}");
    assert!(e.to_string().contains("first"), "{e}");
}

#[test]
fn sv_t9_the_archive_reads_back_with_the_kernels_reader() {
    let files = vec![
        ("bin/a".to_string(), vec![1u8, 2, 3]),
        ("bin/sub/b".to_string(), vec![]),
        ("manifest".to_string(), b"moruna-manifest 1\n".to_vec()),
    ];
    let bytes = archive(&files).expect("archive");
    let read = cpio::files(&bytes).expect("reads");
    assert_eq!(read.len(), 3);
    assert_eq!(cpio::find(&read, "bin/sub/b"), Some(&[][..]));
    assert_eq!(cpio::find(&read, "bin/a"), Some(&[1u8, 2, 3][..]));
}

#[test]
fn sv_t9_build_checks_every_program_and_writes_the_image() {
    let dir = programs(&["supervisor", "child", "ping", "pong-program"]);
    let image = build(DESCRIPTION, &dir).expect("builds");
    let read = cpio::files(&image.initramfs).expect("reads");
    assert_eq!(
        cpio::find(&read, "manifest"),
        Some(image.manifest.as_bytes())
    );
    assert!(cpio::find(&read, "bin/pong-program").is_some());
    assert_eq!(image.expect.len(), 7);
    fs::write(dir.join("child"), b"not an elf").expect("write");
    let e = build(DESCRIPTION, &dir).err().expect("refused");
    assert!(
        e.reason
            .contains("program `child` is not an image the kernel loads"),
        "{e}"
    );
    fs::remove_file(dir.join("child")).expect("remove");
    let e = build(DESCRIPTION, &dir).err().expect("refused");
    assert!(e.reason.starts_with("program `child`:"), "{e}");
    assert!(build("moruna-image 9", &dir).is_err());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn sv_t9_the_command_writes_the_image_and_says_why_it_cannot() {
    let tool = env!("CARGO_BIN_EXE_moruna-image");
    let dir = programs(&["supervisor", "child", "ping", "pong-program", "cli"]);
    let description = dir.join("boot.image");
    fs::write(&description, DESCRIPTION).expect("description");
    let out = dir.join("out");
    let run = Command::new(tool)
        .args([&description, &dir, &out])
        .output()
        .expect("runs");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(out.join("initramfs.cpio").is_file());
    assert_eq!(
        fs::read_to_string(out.join("expect"))
            .expect("expect")
            .lines()
            .count(),
        7
    );
    assert!(
        fs::read_to_string(out.join("manifest"))
            .expect("manifest")
            .starts_with("moruna-manifest 1")
    );
    let usage = Command::new(tool).output().expect("runs");
    assert_eq!(usage.status.code(), Some(2));
    let missing = Command::new(tool)
        .args([&dir.join("nothing"), &dir, &out])
        .output()
        .expect("runs");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).starts_with("moruna-image: "));
    fs::write(&description, "moruna-image 1\nprocess ghost\n").expect("description");
    let ghost = Command::new(tool)
        .args([&description, &dir, &out])
        .output()
        .expect("runs");
    assert!(String::from_utf8_lossy(&ghost.stderr).contains("program `ghost`"));
    let blocked = Command::new(tool)
        .args([&dir.join("boot.image"), &dir, &dir.join("cli")])
        .output()
        .expect("runs");
    assert!(!blocked.status.success());
    fs::remove_dir_all(dir).expect("clean");
}
