//! stream U: the usage build's evidence, collected at the end of a job before the node is
//! stopped (feature `usage`; `kernel/usage`, `architecture/data-os-streams.md`).
//!
//! No production build has this module. In the usage build the kernel and the node's programs
//! are compiled with coverage counters. The programs print theirs on the serial console when
//! they go idle; the kernel's stay in its image. So, once the job has ended, this:
//!
//! 1. waits for the console to settle, which gives every program time to go idle and print
//!    its last dump;
//! 2. pauses the guest over the node's own QMP socket and has QEMU save the kernel's counter
//!    section (`pmemsave`, at the physical address the kernel ELF's segments give);
//! 3. keeps the counters, the serial log and the kernel ELF in the usage directory.
//!
//! Nothing here runs in the guest or gives the guest anything: the host reads what is already
//! there, through the socket the launcher owns.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use moruna_usage::{elf::Elf, profraw};

use crate::error::{Result, VmmError};

/// How long to wait for the console to settle.
#[derive(Clone, Copy, Debug)]
pub struct Settle {
    /// Waited in any case: a program dumps within a second of going idle, and a busy one
    /// within five seconds (`moruna_sys::usage`).
    pub least: Duration,
    /// The console must have been still this long.
    pub quiet: Duration,
    /// Never waited longer.
    pub most: Duration,
}

impl Default for Settle {
    fn default() -> Settle {
        Settle {
            least: Duration::from_secs(8),
            quiet: Duration::from_secs(2),
            most: Duration::from_secs(60),
        }
    }
}

/// What one collection kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    /// The directory it wrote.
    pub dir: PathBuf,
    /// The kernel's counter section's virtual address.
    pub vaddr: u64,
    /// Its physical address in the guest.
    pub paddr: u64,
    /// Its size in bytes.
    pub size: u64,
    /// How long the console took to settle.
    pub waited: Duration,
    /// Whether it settled before `most`.
    pub settled: bool,
}

fn bad(msg: impl Into<String>) -> VmmError {
    VmmError::Control(format!("usage: {}", msg.into()))
}

/// Waits until the file at `path` has not grown for `settle.quiet`, and `settle.least` has
/// passed, or `settle.most` has. Returns the time waited and whether it settled.
pub fn settle(path: &Path, settle: Settle) -> (Duration, bool) {
    let begun = Instant::now();
    let size = || fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut last = size();
    let mut still_since = Instant::now();
    loop {
        let waited = begun.elapsed();
        if waited >= settle.least && still_since.elapsed() >= settle.quiet {
            return (waited, true);
        }
        if waited >= settle.most {
            return (waited, false);
        }
        std::thread::sleep(Duration::from_millis(50).min(settle.quiet));
        let now = size();
        if now != last {
            last = now;
            still_since = Instant::now();
        }
    }
}

/// The kernel's counter section in a kernel ELF: (virtual address, physical address, size).
pub fn kernel_counters(elf: &[u8]) -> Result<(u64, u64, u64)> {
    let elf = Elf::parse(elf).map_err(|e| bad(format!("kernel ELF: {e}")))?;
    let counters = profraw::counters(&elf).map_err(|e| bad(format!("kernel ELF: {e}")))?;
    let paddr = elf
        .paddr(counters.addr, counters.size)
        .ok_or_else(|| bad("the kernel's counters are in no loadable segment"))?;
    Ok((counters.addr, paddr, counters.size))
}

/// A client of the node's QMP socket.
struct Qmp {
    reader: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(path: &Path, timeout: Duration) -> Result<Qmp> {
        let stream =
            UnixStream::connect(path).map_err(|e| VmmError::io("connect", "usage QMP", &e))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(|e| VmmError::io("timeout", "usage QMP", &e))?;
        let mut qmp = Qmp {
            reader: BufReader::new(stream),
        };
        if qmp.reply()?.get("QMP").is_none() {
            return Err(bad("QMP gave no greeting"));
        }
        qmp.execute(serde_json::json!({"execute": "qmp_capabilities"}))?;
        Ok(qmp)
    }

    /// The next frame that is not an event.
    fn reply(&mut self) -> Result<serde_json::Value> {
        for _ in 0..64 {
            let mut line = String::new();
            let count = Read::by_ref(&mut self.reader)
                .take(64 * 1024)
                .read_line(&mut line)
                .map_err(|e| VmmError::io("read", "usage QMP", &e))?;
            if count == 0 || !line.ends_with('\n') {
                return Err(bad("QMP closed or sent an unbounded frame"));
            }
            let value: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| bad(format!("QMP sent no JSON: {e}")))?;
            if value.get("event").is_none() {
                return Ok(value);
            }
        }
        Err(bad("QMP sent only events"))
    }

    fn execute(&mut self, command: serde_json::Value) -> Result<serde_json::Value> {
        let mut text = command.to_string();
        text.push_str("\r\n");
        self.reader
            .get_mut()
            .write_all(text.as_bytes())
            .map_err(|e| VmmError::io("write", "usage QMP", &e))?;
        let reply = self.reply()?;
        match reply.get("return") {
            Some(value) => Ok(value.clone()),
            None => Err(bad(format!("QMP refused {command}: {reply}"))),
        }
    }
}

/// What `RunningNode::shutdown` calls in the usage build (`usage_hook!`): when
/// `MORUNA_USAGE_DIR` names a directory, the node's evidence is collected into it before the
/// node is stopped. Every outcome is said on standard error; none stops the shutdown.
pub fn at_shutdown(runtime_dir: &Path) {
    at_shutdown_with(
        runtime_dir,
        std::env::var_os("MORUNA_USAGE_DIR"),
        Settle::default(),
    );
}

fn at_shutdown_with(runtime_dir: &Path, out: Option<std::ffi::OsString>, wait: Settle) {
    let Some(out) = out.filter(|o| !o.is_empty()) else {
        return;
    };
    match collect(runtime_dir, Path::new(&out), wait) {
        Ok(c) => eprintln!(
            "moruna usage: {} bytes of kernel counters and the serial log in {} (console {} after {:.1} s)",
            c.size,
            c.dir.display(),
            if c.settled {
                "settled"
            } else {
                "still writing"
            },
            c.waited.as_secs_f64()
        ),
        Err(why) => eprintln!("moruna usage: not collected: {why}"),
    }
}

/// Collects the node's usage evidence into a new directory `out`, from the node's runtime
/// directory (`RunningNode::runtime_dir`): its `serial.log`, its `kernel.elf` and its QMP
/// socket `qmp/qmp.sock`. Leaves the guest paused; the node's shutdown ends it.
pub fn collect(runtime_dir: &Path, out: &Path, wait: Settle) -> Result<Collected> {
    let serial = runtime_dir.join("serial.log");
    let (waited, settled) = settle(&serial, wait);
    let kernel = fs::read(runtime_dir.join("kernel.elf"))
        .map_err(|e| VmmError::io("read", "the node's kernel", &e))?;
    let (vaddr, paddr, size) = kernel_counters(&kernel)?;
    fs::create_dir_all(out).map_err(|e| VmmError::io("create", out.display().to_string(), &e))?;
    // QEMU writes the counters into the directory of its QMP socket, the one place its jail
    // lets it write besides the serial log.
    let qmp_dir = runtime_dir.join("qmp");
    let saved = qmp_dir.join("kernel.cnts");
    let mut qmp = Qmp::connect(&qmp_dir.join("qmp.sock"), Duration::from_secs(30))?;
    qmp.execute(serde_json::json!({"execute": "stop"}))?;
    qmp.execute(serde_json::json!({"execute": "pmemsave", "arguments": {
        "val": paddr, "size": size, "filename": saved.display().to_string()}}))?;
    drop(qmp);
    let counters = fs::read(&saved).map_err(|e| VmmError::io("read", "saved counters", &e))?;
    if counters.len() as u64 != size {
        return Err(bad(format!(
            "QEMU saved {} bytes of the kernel's {size} bytes of counters",
            counters.len()
        )));
    }
    let write = |name: &str, bytes: &[u8]| {
        let path = out.join(name);
        fs::write(&path, bytes).map_err(|e| VmmError::io("write", path.display().to_string(), &e))
    };
    write("kernel.cnts", &counters)?;
    // The launcher's copy says EM_386 for QEMU's loader; the kept copy is the kernel again.
    let mut kernel = kernel;
    if kernel.len() >= 20 {
        kernel[18..20].copy_from_slice(&62u16.to_le_bytes());
    }
    write("kernel.elf", &kernel)?;
    fs::copy(&serial, out.join("serial.log"))
        .map_err(|e| VmmError::io("copy", "serial log", &e))?;
    let facts = serde_json::json!({
        "version": 1,
        "kernel_counters": {"vaddr": vaddr, "paddr": paddr, "size": size},
        "settle": {"waited_s": waited.as_secs_f64(), "settled": settled},
    });
    write("usage.json", facts.to_string().as_bytes())?;
    let _ = fs::remove_file(&saved);
    Ok(Collected {
        dir: out.to_path_buf(),
        vaddr,
        paddr,
        size,
        waited,
        settled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    const CNTS: u64 = 0xffff_ffff_883e_e5b8;

    /// A kernel ELF with a `__llvm_prf_cnts` section of `size` bytes in a segment loaded at
    /// physical `virtual - 0xffff_ffff_8000_0000`.
    fn kernel(size: u64) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        out[18..20].copy_from_slice(&3u16.to_le_bytes());
        let names = b"\0__llvm_prf_cnts\0.shstrtab\0";
        let names_at = out.len() as u64;
        out.extend_from_slice(names);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        let phoff = out.len() as u64;
        let mut ph = vec![0u8; 56];
        ph[..4].copy_from_slice(&1u32.to_le_bytes());
        ph[16..24].copy_from_slice(&(CNTS & !0xfff).to_le_bytes());
        ph[24..32].copy_from_slice(&((CNTS & !0xfff) - 0xffff_ffff_8000_0000).to_le_bytes());
        ph[40..48].copy_from_slice(&(size + 0x1000).to_le_bytes());
        out.extend(ph);
        let shoff = out.len() as u64;
        out.extend(vec![0u8; 64]);
        let mut cnts = vec![0u8; 64];
        cnts[..4].copy_from_slice(&1u32.to_le_bytes());
        cnts[4..8].copy_from_slice(&8u32.to_le_bytes());
        cnts[16..24].copy_from_slice(&CNTS.to_le_bytes());
        cnts[32..40].copy_from_slice(&size.to_le_bytes());
        out.extend(cnts);
        let mut shstr = vec![0u8; 64];
        shstr[..4].copy_from_slice(&17u32.to_le_bytes());
        shstr[4..8].copy_from_slice(&3u32.to_le_bytes());
        shstr[24..32].copy_from_slice(&names_at.to_le_bytes());
        shstr[32..40].copy_from_slice(&(names.len() as u64).to_le_bytes());
        out.extend(shstr);
        out[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        out[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        out[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&2u16.to_le_bytes());
        out
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("moruna-usage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("qmp")).unwrap();
        dir
    }

    /// A QEMU stand-in on `socket`: a greeting, an event before each answer, and `pmemsave`
    /// writing `fill` bytes (`short` fewer than asked). Returns the commands it was sent.
    fn fake_qemu(
        socket: &Path,
        refuse: Option<&'static str>,
        short: u64,
    ) -> std::thread::JoinHandle<Vec<serde_json::Value>> {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            writer.write_all(b"{\"QMP\": {\"version\": {}}}\n").unwrap();
            let mut seen = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return seen;
                }
                let command: serde_json::Value = serde_json::from_str(&line).unwrap();
                let name = command["execute"].as_str().unwrap().to_string();
                if name == "pmemsave" {
                    let args = &command["arguments"];
                    let size = args["size"].as_u64().unwrap() - short;
                    fs::write(args["filename"].as_str().unwrap(), vec![7u8; size as usize])
                        .unwrap();
                }
                seen.push(command);
                writer.write_all(b"{\"event\": \"STOP\"}\n").unwrap();
                if refuse == Some(name.as_str()) {
                    writer
                        .write_all(b"{\"error\": {\"class\": \"GenericError\"}}\n")
                        .unwrap();
                } else {
                    writer.write_all(b"{\"return\": {}}\n").unwrap();
                }
            }
        })
    }

    fn quick() -> Settle {
        Settle {
            least: Duration::from_millis(50),
            quiet: Duration::from_millis(30),
            most: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_kernels_counters_are_saved_with_the_serial_log_and_the_kernel() {
        let run = scratch("collect");
        fs::write(run.join("kernel.elf"), kernel(4096)).unwrap();
        fs::write(run.join("serial.log"), b"#### usage 1 b ...\n").unwrap();
        let qemu = fake_qemu(&run.join("qmp/qmp.sock"), None, 0);
        let out = run.join("out");
        let got = collect(&run, &out, quick()).unwrap();
        let seen = qemu.join().unwrap();
        assert_eq!(seen[0]["execute"], "qmp_capabilities");
        assert_eq!(seen[1]["execute"], "stop");
        assert_eq!(seen[2]["arguments"]["val"], CNTS - 0xffff_ffff_8000_0000);
        assert_eq!(seen[2]["arguments"]["size"], 4096);
        assert_eq!((got.vaddr, got.paddr, got.size), (CNTS, 0x83e_e5b8, 4096));
        assert!(got.settled);
        assert_eq!(fs::read(out.join("kernel.cnts")).unwrap(), vec![7u8; 4096]);
        assert_eq!(&fs::read(out.join("kernel.elf")).unwrap()[18..20], &[62, 0]);
        assert!(
            fs::read_to_string(out.join("serial.log"))
                .unwrap()
                .starts_with("#### usage")
        );
        let facts: serde_json::Value =
            serde_json::from_slice(&fs::read(out.join("usage.json")).unwrap()).unwrap();
        assert_eq!(facts["kernel_counters"]["size"], 4096);
        assert!(!run.join("qmp/kernel.cnts").exists());
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_refusal_or_a_short_save_is_an_error() {
        let run = scratch("refuse");
        fs::write(run.join("kernel.elf"), kernel(64)).unwrap();
        let qemu = fake_qemu(&run.join("qmp/qmp.sock"), Some("stop"), 0);
        let err = collect(&run, &run.join("out"), quick()).unwrap_err();
        assert!(err.to_string().contains("QMP refused"), "{err}");
        qemu.join().unwrap();
        fs::remove_file(run.join("qmp/qmp.sock")).unwrap();
        let qemu = fake_qemu(&run.join("qmp/qmp.sock"), None, 8);
        let err = collect(&run, &run.join("out"), quick()).unwrap_err();
        assert!(err.to_string().contains("saved 56 bytes"), "{err}");
        qemu.join().unwrap();
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_kernel_without_counters_or_a_missing_socket_is_an_error() {
        let run = scratch("plain");
        let mut plain = kernel(64);
        // Rename the section, so the kernel has no counters.
        let at = plain
            .windows(15)
            .position(|w| w == b"__llvm_prf_cnts")
            .unwrap();
        plain[at] = b'X';
        fs::write(run.join("kernel.elf"), &plain).unwrap();
        let err = collect(&run, &run.join("out"), quick()).unwrap_err();
        assert!(err.to_string().contains("not built with coverage"), "{err}");
        assert!(
            kernel_counters(b"MZ")
                .unwrap_err()
                .to_string()
                .contains("not an ELF")
        );
        // A counter section bigger than its segment.
        let mut far = kernel(64);
        let ph = u64::from_le_bytes(far[0x20..0x28].try_into().unwrap()) as usize;
        far[ph + 40..ph + 48].copy_from_slice(&16u64.to_le_bytes());
        assert!(
            kernel_counters(&far)
                .unwrap_err()
                .to_string()
                .contains("no loadable")
        );
        fs::write(run.join("kernel.elf"), kernel(64)).unwrap();
        let err = collect(&run, &run.join("out"), quick()).unwrap_err();
        assert!(err.to_string().contains("usage QMP"), "{err}");
        assert!(collect(&run.join("none"), &run.join("out"), quick()).is_err());
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn the_shutdown_hook_collects_only_when_asked() {
        let run = scratch("hook");
        fs::write(run.join("kernel.elf"), kernel(64)).unwrap();
        // Not asked: nothing is touched.
        at_shutdown_with(&run, None, quick());
        at_shutdown_with(&run, Some("".into()), quick());
        assert!(!run.join("out").exists());
        // Asked, with no QEMU: said, and the shutdown goes on.
        at_shutdown_with(&run, Some(run.join("out").into()), quick());
        assert!(run.join("out").is_dir());
        // Asked, with QEMU.
        let qemu = fake_qemu(&run.join("qmp/qmp.sock"), None, 0);
        at_shutdown_with(&run, Some(run.join("kept").into()), quick());
        qemu.join().unwrap();
        assert!(run.join("kept/kernel.cnts").is_file());
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn the_console_settles_when_it_stops_growing_or_gives_up() {
        let run = scratch("settle");
        let log = run.join("serial.log");
        fs::write(&log, b"a").unwrap();
        let writer = {
            let log = log.clone();
            std::thread::spawn(move || {
                for _ in 0..6 {
                    std::thread::sleep(Duration::from_millis(20));
                    let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
                    f.write_all(b"b").unwrap();
                }
            })
        };
        let (waited, settled) = settle(&log, quick());
        writer.join().unwrap();
        assert!(settled);
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        let never = Settle {
            least: Duration::from_millis(10),
            quiet: Duration::from_secs(10),
            most: Duration::from_millis(60),
        };
        assert!(!settle(&log, never).1);
        assert_eq!(Settle::default().least, Duration::from_secs(8));
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_qmp_that_closes_or_speaks_no_json_is_an_error() {
        let run = scratch("qmp");
        let socket = run.join("qmp.sock");
        for greeting in ["", "not json\n", "{\"return\": {}}\n"] {
            let _ = fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.write_all(greeting.as_bytes()).unwrap();
            });
            assert!(
                Qmp::connect(&socket, Duration::from_secs(2)).is_err(),
                "{greeting}"
            );
            server.join().unwrap();
        }
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for _ in 0..70 {
                stream.write_all(b"{\"event\": \"X\"}\n").unwrap();
            }
        });
        let err = Qmp::connect(&socket, Duration::from_secs(2)).err().unwrap();
        assert!(err.to_string().contains("only events"), "{err}");
        server.join().unwrap();
        let _ = fs::remove_dir_all(&run);
    }
}
