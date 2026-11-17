//! Loading a program and mapping files (SDD 28, f.2 and f.3): what Linux's `execve`
//! and `mmap` of a file do, over the image filesystem and the calls on another
//! process's memory (`process_map`, `process_write`, `process_protect`).
//!
//! A segment or a mapping the process only reads (code, read-only data) gets the image
//! filesystem's own pages, read only, shared by every task that maps the same range; a
//! private mapping it may write gets a copy of the bytes, in zero pages of its own;
//! pages past the file's end are zero. Only the range asked for is read.

use alloc::vec::Vec;

use imagefs_protocol::{Bytes as FsBytes, READ_MAX, kind};
use moruna_abi::map;
use moruna_linux::{
    elf::{self, Plan},
    mmap::{self, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE},
    stack::{self, Start, at},
    types::errno,
};
use moruna_svc::Error;

use crate::{
    files::{FsFile, Open},
    memory,
    task::Task,
};

const PAGE: u64 = 4096;
/// The lowest address a program is mapped at.
pub const USER_START: u64 = 0x1000;
/// The end of user space.
pub const USER_END: u64 = 0x0000_7fff_ffff_f000;
/// Where a position-independent program goes, as Linux's `ELF_ET_DYN_BASE`.
const PROGRAM_BASE: u64 = 0x0000_5555_5555_4000;
/// Where the interpreter goes.
const INTERPRETER_BASE: u64 = 0x0000_7f00_0000_0000;
/// The main thread's stack: its top, and its size (`RLIMIT_STACK`).
const STACK_TOP: u64 = 0x0000_7fff_f000_0000;
const STACK_SIZE: u64 = 8 << 20;

/// Where a loaded program starts.
pub struct Image {
    pub entry: u64,
    pub stack: u64,
    pub brk: u64,
}

fn prot_of(read: bool, write: bool, execute: bool) -> u64 {
    let mut p = 0;
    if read {
        p |= map::READ;
    }
    if write {
        p |= map::WRITE;
    }
    if execute {
        p |= map::EXECUTE;
    }
    p
}

fn errno_of(e: Error) -> u64 {
    match e {
        Error::NoMemory | Error::Limit | Error::OutOfRange | Error::QuotaExceeded => errno::ENOMEM,
        Error::AccessDenied => errno::EACCES,
        Error::BadState => errno::EEXIST,
        Error::Fault => errno::EFAULT,
        _ => errno::EINVAL,
    }
}

/// An image file open for loading: its number and size.
struct Exe {
    number: u32,
    size: u64,
}

impl Task {
    fn open_exe(&mut self, path: &[u8]) -> Result<Exe, u64> {
        let (number, stat) = self
            .fs
            .open(FsBytes(path.to_vec()), 0, 0)
            .map_err(crate::files::errno_of)?;
        if stat.kind != kind::REGULAR || stat.mode & 0o111 == 0 {
            let _ = self.fs.close(number);
            return Err(errno::EACCES);
        }
        Ok(Exe {
            number,
            size: stat.size,
        })
    }

    fn read_file(&mut self, number: u32, offset: u64, len: u64) -> Result<Vec<u8>, u64> {
        let mut out = Vec::new();
        while (out.len() as u64) < len {
            let want = (len - out.len() as u64).min(READ_MAX as u64) as u32;
            let FsBytes(chunk) = self
                .fs
                .read(number, offset + out.len() as u64, want)
                .map_err(crate::files::errno_of)?;
            if chunk.is_empty() {
                return Err(errno::ENOEXEC);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    fn plan_of(&mut self, exe: &Exe) -> Result<Plan, u64> {
        let ehdr = self.read_file(exe.number, 0, 64.min(exe.size))?;
        let (phoff, len) = elf::headers(&ehdr).map_err(|_| errno::ENOEXEC)?;
        let phdrs = self.read_file(exe.number, phoff, len)?;
        elf::parse(&ehdr, &phdrs, exe.size).map_err(|_| errno::ENOEXEC)
    }

    /// Whether `path` is a program this personality loads, before `execve` lets the
    /// old one go.
    pub fn check_executable(&mut self, path: &[u8]) -> Result<(), u64> {
        let exe = self.open_exe(path)?;
        let plan = self.plan_of(&exe);
        let _ = self.fs.close(exe.number);
        plan.map(|_| ())
    }

    /// Maps the segments of `plan` at `bias`.
    fn map_segments(&mut self, exe: &Exe, plan: &Plan, bias: u64) -> Result<(), u64> {
        for s in &plan.segments {
            let start = bias + s.page_start();
            let len = s.page_end() - s.page_start();
            let prot = prot_of(s.read, s.write, s.execute);
            self.map_file(
                Some(start),
                len,
                prot,
                MAP_PRIVATE | MAP_FIXED,
                exe.number,
                exe.size,
                true,
                s.file_page(),
                Some(s.file_span()),
            )?;
        }
        Ok(())
    }

    /// Loads the program at `path` into the task's empty address space, with its
    /// interpreter, and builds its first stack: what `execve` does.
    pub fn load(&mut self, path: &[u8], argv: &[Vec<u8>], env: &[Vec<u8>]) -> Result<Image, u64> {
        let exe = self.open_exe(path)?;
        let loaded = self.load_open(&exe, path, argv, env);
        let _ = self.fs.close(exe.number);
        loaded
    }

    fn load_open(
        &mut self,
        exe: &Exe,
        path: &[u8],
        argv: &[Vec<u8>],
        env: &[Vec<u8>],
    ) -> Result<Image, u64> {
        let plan = self.plan_of(exe)?;
        let (low, high) = plan.span();
        let bias = if plan.relocatable {
            PROGRAM_BASE - low
        } else {
            0
        };
        self.map_segments(exe, &plan, bias)?;
        let brk = bias + high;
        let mut entry = bias + plan.entry;
        let mut interp_base = 0;
        if let Some((offset, len)) = plan.interp {
            let bytes = self.read_file(exe.number, offset, len)?;
            let ipath = elf::interpreter(&bytes)
                .map_err(|_| errno::ENOEXEC)?
                .to_vec();
            let interp = self.open_exe(&ipath)?;
            let iplan = self.plan_of(&interp);
            let mapped = match iplan {
                Ok(iplan) => {
                    let (ilow, _) = iplan.span();
                    let ibias = if iplan.relocatable {
                        INTERPRETER_BASE - ilow
                    } else {
                        0
                    };
                    self.map_segments(&interp, &iplan, ibias)
                        .map(|_| (ibias, iplan.entry))
                }
                Err(e) => Err(e),
            };
            let _ = self.fs.close(interp.number);
            let (ibias, ientry) = mapped?;
            interp_base = ibias;
            entry = ibias + ientry;
        }
        // The stack: zero pages, filled as they are touched.
        self.process
            .map(
                None,
                0,
                STACK_SIZE,
                STACK_TOP - STACK_SIZE,
                map::READ | map::WRITE | map::FIXED,
            )
            .map_err(errno_of)?;
        let mut random = [0u8; 16];
        moruna_sys::random(&mut random).map_err(|_| errno::EIO)?;
        let auxv = [
            (at::PHDR, bias + plan.phdr),
            (at::PHENT, 56),
            (at::PHNUM, plan.phnum),
            (at::PAGESZ, PAGE),
            (at::BASE, interp_base),
            (at::FLAGS, 0),
            (at::ENTRY, bias + plan.entry),
            (at::UID, 0),
            (at::EUID, 0),
            (at::GID, 0),
            (at::EGID, 0),
            (at::HWCAP, 0),
            (at::HWCAP2, 0),
            (at::CLKTCK, 100),
            (at::SECURE, 0),
        ];
        let mut auxv = auxv.to_vec();
        // A granted program's start page (SDD 30 d.2).
        if let Some(page) = self.start_page {
            auxv.push((linux_protocol::AT_MORUNA_START, page));
        }
        let argv: Vec<&[u8]> = argv.iter().map(|a| a.as_slice()).collect();
        let envp: Vec<&[u8]> = env.iter().map(|e| e.as_slice()).collect();
        let start = Start {
            argv: &argv,
            envp: &envp,
            execfn: path,
            random,
            auxv: &auxv,
        };
        let (sp, bytes) = stack::build(STACK_TOP, &start).ok_or(errno::E2BIG)?;
        memory::write(&self.process, sp, &bytes)?;
        Ok(Image {
            entry,
            stack: sp,
            brk,
        })
    }

    /// `mmap` of a file, forwarded because the kernel's fast `mmap` takes only
    /// anonymous memory.
    pub fn mmap(&mut self, a: [u64; 6]) -> Result<u64, u64> {
        let [addr, len, prot, flags, fd, offset] = a;
        let entry = self.fds.get(fd).ok_or(errno::EBADF)?.clone();
        let file = entry.file.borrow();
        let (number, size, mappable) = match &*file {
            Open::Fs(FsFile {
                number, stat, path, ..
            }) => {
                if stat.kind != kind::REGULAR {
                    return Err(errno::ENODEV);
                }
                let assets = self.fs.is_assets(*number);
                if assets && prot & map::WRITE != 0 {
                    return Err(errno::EACCES);
                }
                (*number, stat.size, !assets && !path.starts_with(b"/tmp/"))
            }
            Open::Device(crate::files::Device::Zero) => {
                drop(file);
                return self.map_zero(addr, len, prot, flags);
            }
            _ => return Err(errno::ENODEV),
        };
        drop(file);
        let fixed = flags & MAP_FIXED != 0;
        let at_ = if fixed || flags & MAP_FIXED_NOREPLACE != 0 || addr != 0 {
            Some(addr)
        } else {
            None
        };
        self.map_file(at_, len, prot, flags, number, size, mappable, offset, None)
    }

    fn map_zero(&mut self, addr: u64, len: u64, prot: u64, flags: u64) -> Result<u64, u64> {
        let len = len.div_ceil(PAGE) * PAGE;
        if len == 0 {
            return Err(errno::EINVAL);
        }
        let fixed = flags & MAP_FIXED != 0;
        let extra = if fixed { map::FIXED } else { 0 };
        self.reserve(
            addr,
            len,
            prot | extra,
            fixed,
            flags & MAP_FIXED_NOREPLACE != 0,
        )
    }

    /// Reserves `len` bytes of zero pages with `flags`: at `addr` exactly when fixed
    /// (`FIXED` in `flags` replaces; without, it is refused when taken), at `addr` when
    /// it is free otherwise, anywhere when not.
    fn reserve(
        &mut self,
        addr: u64,
        len: u64,
        flags: u64,
        fixed: bool,
        noreplace: bool,
    ) -> Result<u64, u64> {
        if fixed || noreplace {
            if !addr.is_multiple_of(PAGE) {
                return Err(errno::EINVAL);
            }
            return self
                .process
                .map(None, 0, len, addr, flags)
                .map_err(errno_of);
        }
        if addr != 0
            && addr.is_multiple_of(PAGE)
            && let Ok(va) = self.process.map(None, 0, len, addr, flags)
        {
            return Ok(va);
        }
        self.process.map(None, 0, len, 0, flags).map_err(errno_of)
    }

    /// Maps `len` bytes of file `number` (of `size` bytes) from `offset` with `prot`
    /// and the `mmap` `flags`, at `at` (exact with `MAP_FIXED`, a hint otherwise) or
    /// anywhere. `limit` bounds the file bytes a copy takes (a segment's file part).
    #[allow(clippy::too_many_arguments)]
    fn map_file(
        &mut self,
        at_: Option<u64>,
        len: u64,
        prot: u64,
        flags: u64,
        number: u32,
        size: u64,
        mappable: bool,
        offset: u64,
        limit: Option<u64>,
    ) -> Result<u64, u64> {
        let assets = self.fs.is_assets(number);
        if assets && prot & map::WRITE != 0 {
            return Err(errno::EACCES);
        }
        let plan = mmap::plan(len, prot, flags, offset, size, mappable && !assets)?;
        let fixed = flags & MAP_FIXED != 0;
        let noreplace = flags & MAP_FIXED_NOREPLACE != 0;
        let first = if plan.copy {
            map::READ | map::WRITE
        } else {
            prot
        };
        let extra = if fixed { map::FIXED } else { 0 };
        let va = self.reserve(at_.unwrap_or(0), plan.len, first | extra, fixed, noreplace)?;
        let done = self.fill(va, &plan, prot, number, size, offset, limit);
        if let Err(e) = done {
            let _ = self.process.unmap(va, plan.len);
            return Err(e);
        }
        Ok(va)
    }

    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        va: u64,
        plan: &mmap::FilePlan,
        prot: u64,
        number: u32,
        size: u64,
        offset: u64,
        limit: Option<u64>,
    ) -> Result<(), u64> {
        if plan.file_len == 0 {
            return Ok(());
        }
        if plan.copy {
            let bytes = limit
                .unwrap_or(plan.file_len)
                .min(plan.file_len)
                .min(size.saturating_sub(offset));
            let mut done = 0u64;
            while done < bytes {
                let want = (bytes - done).min(READ_MAX as u64) as u32;
                let FsBytes(chunk) = self
                    .fs
                    .read(number, offset + done, want)
                    .map_err(crate::files::errno_of)?;
                if chunk.is_empty() {
                    break;
                }
                memory::write(&self.process, va + done, &chunk)?;
                done += chunk.len() as u64;
            }
            if prot & map::WRITE == 0 {
                self.process.protect(va, plan.len, prot).map_err(errno_of)?;
            }
            return Ok(());
        }
        let (memory, memory_offset) = self
            .fs
            .map_readonly(number, offset, plan.file_len)
            .map_err(crate::files::errno_of)?;
        self.process
            .map(
                Some(&memory.0),
                memory_offset,
                plan.file_len,
                va,
                prot | map::FIXED,
            )
            .map_err(errno_of)?;
        Ok(())
    }
}
