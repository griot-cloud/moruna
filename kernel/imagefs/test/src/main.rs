//! IF-T12 to IF-T17 (SDD 27, k): a client of the image filesystem, over the two layer
//! disks the CI boot attaches (`moruna-layer fixture`, SDD 27 f.1), with the host's
//! list of what the merged tree holds (the slot `checks`):
//!
//! - the cold start: one 4 KiB file of the 100 MiB layer read first, and the pages
//!   that cost counted (IF-T12);
//! - every entry stat'ed, every directory listed, every file read and its SHA-256
//!   checked, every link read, against the host's list (IF-T13);
//! - `map` gives the bytes `read` gives, and a second mapping of the same pages is
//!   the same memory, read from no device (IF-T14);
//! - a second client reads a file the first read with no block read (IF-T15);
//! - the scratch directory refuses a write past its quota, and is the task's own;
//!   nothing is written to the image (IF-T16);
//! - the protocol's conformance suite (IF-T17).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use imagefs_protocol::{Bytes, READ_MAX, Stat, Stats, imagefs, kind, open};
use moruna_imagefs::sha256;
use moruna_svc::{CallError, Channel, Console, Memory, conformance, deadline_in, println, start};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;
/// The most pages the cold read may cost: SDD 27 f.5 counts them.
const COLD_PAGES_MAX: u64 = 10;
const PAGE: u64 = 4096;

type Client = imagefs::Client;

fn connect(listener: &Channel) -> Result<Client, String> {
    let channel = moruna_svc::connect(listener).map_err(|e| format!("connecting: {e:?}"))?;
    let mut c = Client::connect(channel, deadline_in(300 * SECONDS))
        .map_err(|e| format!("hello to imagefs: {e:?}"))?;
    c.core().set_timeout(300 * SECONDS);
    Ok(c)
}

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One line of the host's list.
enum Check {
    Dir { mode: u32 },
    File { mode: u32, size: u64, sha: String },
    Link { target: String },
}

struct Checks {
    cold: String,
    big: String,
    entries: BTreeMap<String, Check>,
}

fn field<'a>(it: &mut impl Iterator<Item = &'a str>, line: &str) -> Result<&'a str, String> {
    it.next()
        .ok_or_else(|| format!("a short line in the list: `{line}`"))
}

fn octal(text: &str) -> Result<u32, String> {
    u32::from_str_radix(text, 8).map_err(|_| format!("`{text}` is not a mode"))
}

/// The list: `moruna-checks 1 BYTES` (the file's length), `cold PATH`, `big PATH`,
/// then `d MODE PATH`, `f MODE SIZE SHA256 PATH` and `l TARGET PATH` lines.
fn checks() -> Result<Checks, String> {
    let memory = Memory(start::take("checks").ok_or("no checks slot")?);
    let head = memory
        .map(0, PAGE, 0)
        .map_err(|e| format!("mapping the list: {}", e.name()))?;
    let mut first = vec![0u8; 64];
    head.read(0, &mut first).map_err(|e| e.name().to_string())?;
    drop(head);
    let line = first.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let line = core::str::from_utf8(line).map_err(|_| "the list is not text")?;
    let len: usize = line
        .strip_prefix("moruna-checks 1 ")
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| format!("the list begins `{line}`"))?;
    let all = memory
        .map(0, (len as u64).div_ceil(PAGE) * PAGE, 0)
        .map_err(|e| format!("mapping the list: {}", e.name()))?;
    let mut bytes = vec![0u8; len];
    all.read(0, &mut bytes).map_err(|e| e.name().to_string())?;
    let text = core::str::from_utf8(&bytes).map_err(|_| "the list is not text")?;
    let mut cold = None;
    let mut big = None;
    let mut entries = BTreeMap::new();
    for line in text.lines().skip(1).filter(|l| !l.is_empty()) {
        let mut it = line.split(' ');
        let tag = field(&mut it, line)?;
        let check = match tag {
            "cold" => {
                cold = Some(field(&mut it, line)?.to_string());
                continue;
            }
            "big" => {
                big = Some(field(&mut it, line)?.to_string());
                continue;
            }
            "d" => Check::Dir {
                mode: octal(field(&mut it, line)?)?,
            },
            "f" => Check::File {
                mode: octal(field(&mut it, line)?)?,
                size: field(&mut it, line)?
                    .parse()
                    .map_err(|_| format!("a size in `{line}`"))?,
                sha: field(&mut it, line)?.to_string(),
            },
            "l" => Check::Link {
                target: field(&mut it, line)?.to_string(),
            },
            other => return Err(format!("`{other}` is not a kind of entry")),
        };
        entries.insert(field(&mut it, line)?.to_string(), check);
    }
    Ok(Checks {
        cold: cold.ok_or("the list names no cold file")?,
        big: big.ok_or("the list names no big file")?,
        entries,
    })
}

fn call<T>(what: &str, r: Result<T, CallError<imagefs::Error>>) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e:?}"))
}

fn stats(c: &mut Client) -> Result<Stats, String> {
    call("stats", c.stats())
}

/// Every byte of `path`, through `read`, with its SHA-256.
fn read_file(c: &mut Client, path: &str) -> Result<(Vec<u8>, String), String> {
    let (file, stat) = call(path, c.open(Bytes::from(path.as_bytes()), 0, 0))?;
    let mut data = Vec::with_capacity(stat.size.min(1 << 20) as usize);
    let mut hash = sha256::Sha256::new();
    let mut at = 0u64;
    loop {
        let Bytes(chunk) = call(path, c.read(file, at, READ_MAX))?;
        if chunk.is_empty() {
            break;
        }
        hash.update(&chunk);
        if data.len() < (1 << 20) {
            data.extend_from_slice(&chunk);
        }
        at += chunk.len() as u64;
    }
    call(path, c.close(file))?;
    if at != stat.size {
        return Err(format!("{path}: read {at} bytes of {}", stat.size));
    }
    Ok((data, hex(&hash.finish())))
}

fn cold_start(c: &mut Client, checks: &Checks) -> Result<String, String> {
    let before = stats(c)?;
    let path = checks.cold.as_str();
    let (file, stat) = call(path, c.open(Bytes::from(path.as_bytes()), 0, 0))?;
    let Bytes(data) = call(path, c.read(file, 0, PAGE as u32))?;
    call(path, c.close(file))?;
    let after = stats(c)?;
    let Some(Check::File { sha, size, .. }) = checks.entries.get(path) else {
        return Err(format!("the list has no file {path}"));
    };
    if stat.size != PAGE || *size != PAGE || hex(&sha256::digest(&data)) != *sha {
        return Err(format!("{path}: {} bytes, not the host's", data.len()));
    }
    let pages = after.pages_read - before.pages_read;
    let requests = after.block_requests - before.block_requests;
    if pages > COLD_PAGES_MAX {
        return Err(format!("reading {path} cold read {pages} pages"));
    }
    Ok(format!(
        "reading {path} ({} bytes) first, from a layer of over 100 MiB, read {pages} pages from the disks in {requests} requests (and {} pages to mount the layers)",
        data.len(),
        before.pages_read
    ))
}

fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "/",
    }
}

fn tree(c: &mut Client, checks: &Checks) -> Result<String, String> {
    let mut children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    children.insert("/", BTreeSet::new());
    for path in checks.entries.keys() {
        let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
        children.entry(parent(path)).or_default().insert(name);
    }
    if let Some(root) = children.get_mut("/") {
        root.extend(["dev", "proc", "tmp"]);
    }
    let (mut dirs, mut files, mut links, mut bytes) = (0, 0, 0, 0u64);
    for (path, check) in &checks.entries {
        let p = Bytes::from(path.as_bytes());
        let stat: Stat = call(path, c.stat(p.clone(), false))?;
        match check {
            Check::Dir { mode } => {
                if stat.kind != kind::DIRECTORY || stat.mode != *mode {
                    return Err(format!(
                        "{path}: {stat:?}, not a directory of mode {mode:o}"
                    ));
                }
                dirs += 1;
            }
            Check::File { mode, size, sha } => {
                if stat.kind != kind::REGULAR || stat.mode != *mode || stat.size != *size {
                    return Err(format!(
                        "{path}: {stat:?}, not a file of {size} bytes, mode {mode:o}"
                    ));
                }
                let (_, got) = read_file(c, path)?;
                if got != *sha {
                    return Err(format!("{path}: its bytes are not the host's"));
                }
                files += 1;
                bytes += size;
            }
            Check::Link { target } => {
                let Bytes(t) = call(path, c.readlink(p))?;
                if stat.kind != kind::SYMLINK || t != target.as_bytes() {
                    return Err(format!("{path}: not a link to {target}"));
                }
                links += 1;
            }
        }
    }
    for (dir, want) in &children {
        let (file, _) = call(dir, c.open(Bytes::from(dir.as_bytes()), open::DIRECTORY, 0))?;
        let mut got = Vec::new();
        let mut cookie = 0;
        loop {
            let (entries, next) = call(dir, c.readdir(file, cookie))?;
            if entries.is_empty() {
                break;
            }
            got.extend(entries.into_iter().map(|(n, _, _)| n.0));
            cookie = next;
        }
        call(dir, c.close(file))?;
        let want: Vec<&[u8]> = want.iter().map(|n| n.as_bytes()).collect();
        if got.iter().map(Vec::as_slice).collect::<Vec<_>>() != want {
            return Err(format!(
                "{dir} lists {} entries, not the host's {}",
                got.len(),
                want.len()
            ));
        }
    }
    Ok(format!(
        "{dirs} directories (each listed as the host's), {files} files ({bytes} bytes, each read whole and its SHA-256 the host's) and {links} links, merged from the two layers"
    ))
}

/// `len` bytes of `path` from `offset`, by `map`, and by `read`.
fn map_and_read(
    c: &mut Client,
    path: &str,
    offset: u64,
    len: u64,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (file, _) = call(path, c.open(Bytes::from(path.as_bytes()), 0, 0))?;
    let memory = call(path, c.map(file, offset, len))?;
    let span = len.div_ceil(PAGE) * PAGE;
    let m = memory
        .map(0, span, 0)
        .map_err(|e| format!("{path}: mapping: {}", e.name()))?;
    let mut mapped = vec![0u8; len as usize];
    m.read(0, &mut mapped).map_err(|e| e.name().to_string())?;
    let mut read = Vec::with_capacity(len as usize);
    while (read.len() as u64) < len {
        let want = (len - read.len() as u64).min(READ_MAX as u64) as u32;
        let Bytes(chunk) = call(path, c.read(file, offset + read.len() as u64, want))?;
        if chunk.is_empty() {
            break;
        }
        read.extend_from_slice(&chunk);
    }
    read.resize(len as usize, 0);
    call(path, c.close(file))?;
    Ok((mapped, read))
}

fn mapping(c: &mut Client, checks: &Checks) -> Result<String, String> {
    let mut mapped = 0;
    let mut pages = 0;
    for (path, check) in &checks.entries {
        let Check::File { size, .. } = check else {
            continue;
        };
        if *size == 0 || *size > (1 << 20) {
            continue;
        }
        let (m, r) = map_and_read(c, path, 0, *size)?;
        if m != r {
            return Err(format!("{path}: map and read give other bytes"));
        }
        mapped += 1;
        pages += size.div_ceil(PAGE);
    }
    let big = checks.big.as_str();
    let Some(Check::File { size, .. }) = checks.entries.get(big) else {
        return Err(format!("the list has no file {big}"));
    };
    let window = 1u64 << 20;
    let offset = (size / 2) / PAGE * PAGE;
    let (m, r) = map_and_read(c, big, offset, window)?;
    if m != r {
        return Err(format!(
            "{big}: a window of 1 MiB maps to other bytes than it reads"
        ));
    }
    let before = stats(c)?;
    let (again, _) = map_and_read(c, big, offset, window)?;
    let after = stats(c)?;
    if again != m {
        return Err(format!("{big}: a second mapping holds other bytes"));
    }
    if after.map_hits != before.map_hits + 1 || after.map_pages_read != before.map_pages_read {
        return Err(format!(
            "a second mapping of the same pages was not the kept one: {before:?} then {after:?}"
        ));
    }
    Ok(format!(
        "{mapped} files ({pages} pages) and 1 MiB of {big} mapped hold what read gives; the same pages mapped again were the same memory, with no page read ({} pages read into mappings, {} copied from the cache)",
        after.map_pages_read, after.map_pages_copied
    ))
}

fn cache(a: &mut Client, b: &mut Client, checks: &Checks) -> Result<String, String> {
    let (path, size) = checks
        .entries
        .iter()
        .filter_map(|(p, c)| match c {
            Check::File { size, .. } if *size > 8 * PAGE && *size <= (1 << 20) => Some((p, *size)),
            _ => None,
        })
        .max_by_key(|(_, s)| *s)
        .ok_or("the list has no file of 32 KiB to 1 MiB")?;
    read_file(a, path)?;
    let before = stats(b)?;
    let (_, sha) = read_file(b, path)?;
    let after = stats(b)?;
    if let Some(Check::File { sha: want, .. }) = checks.entries.get(path)
        && *want != sha
    {
        return Err(format!("{path}: the second client read other bytes"));
    }
    let hits = after.hits - before.hits;
    if after.block_requests != before.block_requests
        || after.pages_read != before.pages_read
        || hits < size.div_ceil(PAGE)
    {
        return Err(format!(
            "a second client's read of {path} went to the disk: {before:?} then {after:?}"
        ));
    }
    Ok(format!(
        "a second client read {path} ({size} bytes) after the first with no block read and {hits} cache hits; the cache holds {} of {} pages after {} evictions",
        after.resident, after.capacity, after.evictions
    ))
}

fn scratch(a: &mut Client, b: &mut Client, checks: &Checks) -> Result<String, String> {
    let quota = 64 * 1024;
    call("quota", b.quota(quota))?;
    let (file, _) = call(
        "/tmp/q",
        b.open(
            Bytes::from(&b"/tmp/q"[..]),
            open::CREATE | open::WRITE,
            0o600,
        ),
    )?;
    let chunk = vec![0x5au8; 16 * 1024];
    let mut written = 0u64;
    let refused = loop {
        match b.write(file, written, Bytes(chunk.clone())) {
            Ok(n) => written += n as u64,
            Err(CallError::Remote(imagefs::Error::NoSpace)) => break true,
            Err(e) => return Err(format!("writing /tmp/q: {e:?}")),
        }
        if written > quota {
            break false;
        }
    };
    if !refused || written + 16 * 1024 <= quota - 1024 {
        return Err(format!("the quota of {quota} took {written} bytes"));
    }
    let left = call("/tmp/q", b.fstat(file))?;
    if left.size != written {
        return Err(format!("a refused write changed /tmp/q to {}", left.size));
    }
    // The first client's scratch directory is its own.
    match a.stat(Bytes::from(&b"/tmp/q"[..]), true) {
        Err(CallError::Remote(imagefs::Error::NotFound)) => {}
        other => return Err(format!("another task sees /tmp/q: {other:?}")),
    }
    // Nothing is written to the image.
    let (some_file, some_dir) = {
        let f = checks
            .entries
            .iter()
            .find(|(_, c)| matches!(c, Check::File { .. }))
            .map(|(p, _)| p.clone())
            .ok_or("no file in the list")?;
        (f.clone(), parent(&f).to_string())
    };
    let ro = |r: Result<(), CallError<imagefs::Error>>, what: &str| match r {
        Err(CallError::Remote(imagefs::Error::ReadOnly)) => Ok(()),
        other => Err(format!("{what}: {other:?}, not ReadOnly")),
    };
    ro(
        b.open(Bytes::from(some_file.as_bytes()), open::WRITE, 0)
            .map(|_| ()),
        "opening an image file to write",
    )?;
    ro(
        b.open(
            Bytes(format!("{some_dir}/kc2-new").into_bytes()),
            open::CREATE | open::WRITE,
            0o644,
        )
        .map(|_| ()),
        "making a file in the image",
    )?;
    ro(
        b.mkdir(Bytes(format!("{some_dir}/kc2-dir").into_bytes()), 0o755),
        "making a directory in the image",
    )?;
    ro(
        b.remove(Bytes::from(some_file.as_bytes())),
        "removing an image file",
    )?;
    ro(b.remove(Bytes::from(&b"/tmp"[..])), "removing /tmp")?;
    ro(
        b.open(Bytes::from(some_file.as_bytes()), open::TRUNCATE, 0)
            .map(|_| ()),
        "truncating an image file",
    )?;
    let (_, sha) = read_file(b, &some_file)?;
    if let Some(Check::File { sha: want, .. }) = checks.entries.get(&some_file)
        && *want != sha
    {
        return Err(format!("{some_file} changed"));
    }
    call("/tmp/q", b.close(file))?;
    Ok(format!(
        "/tmp took {written} bytes of a {quota}-byte quota and refused the next write with NoSpace; another task does not see it; writing, making, removing and truncating in the image were each refused with ReadOnly, and {some_file} reads as before"
    ))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listener) = start::take("fs").map(Channel) else {
        println!(out, "KC2 imagefs-test: FAIL: no fs slot");
        return 1;
    };
    let checks = match checks() {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KC2 imagefs-test: FAIL: the host's list: {}", e);
            return 1;
        }
    };
    let mut a = match connect(&listener) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KC2 imagefs-test: FAIL: {}", e);
            return 1;
        }
    };
    let mut failed = 0;
    let mut report = |name: &str, r: Result<String, String>| match r {
        Ok(what) => println!(out, "KC2 {}: ok: {}", name, what),
        Err(why) => {
            println!(out, "KC2 {}: FAIL: {}", name, why);
            failed += 1;
        }
    };
    report("cold start", cold_start(&mut a, &checks));
    report("tree", tree(&mut a, &checks));
    report("map", mapping(&mut a, &checks));
    match connect(&listener) {
        Ok(mut b) => {
            report("cache", cache(&mut a, &mut b, &checks));
            report("scratch", scratch(&mut a, &mut b, &checks));
        }
        Err(e) => report("cache", Err(e)),
    }
    drop(a);
    let suite = imagefs_protocol::suite();
    let mut bad = 0;
    let summary = conformance::run(&suite, &mut conformance::Listener(listener), &mut |check| {
        if let Err(why) = check.result {
            println!(out, "KC2 conformance: FAIL: {}: {}", check.name, why);
            bad += 1;
        }
    });
    if summary.ok() && bad == 0 {
        println!(
            out,
            "KC2 conformance {}: ok: {} checks passed against imagefs",
            suite.id.name,
            summary.passed
        );
    } else {
        failed += 1;
    }
    failed
}
