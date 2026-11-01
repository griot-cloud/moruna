//! Whether this process is on the Moruna kernel, and the handles it started with (SDD 30 d.2).
//!
//! The personality puts a granted program's start page in its auxiliary vector under
//! `AT_MORUNA_START`. On a Linux node no such entry exists, `getauxval` answers zero and the
//! engine makes no call of the kernel's: it is an ordinary Linux process there.

use std::sync::OnceLock;

use linux_protocol::AT_MORUNA_START;

/// This process on the Moruna kernel.
#[derive(Debug)]
pub struct Kernel {
    /// The ABI the kernel speaks, `MAJOR << 16 | MINOR`.
    pub abi: u64,
    /// The CPUs online, from the start page.
    pub cpus: usize,
}

static KERNEL: OnceLock<Option<Kernel>> = OnceLock::new();

/// The kernel this process runs on, when it was started with a start page; the page is read
/// once, the first time anything asks.
pub fn kernel() -> Option<&'static Kernel> {
    KERNEL
        .get_or_init(|| {
            // SAFETY: `getauxval` reads the auxiliary vector the C library keeps from the
            // process's start; it takes any type and answers zero for one that is absent.
            #[allow(unsafe_code)]
            let page = unsafe { libc::getauxval(AT_MORUNA_START as libc::c_ulong) };
            if page == 0 {
                return None;
            }
            moruna_sys::start::init(page);
            Some(Kernel {
                abi: moruna_sys::abi_version(),
                cpus: moruna_sys::start::cpus(),
            })
        })
        .as_ref()
}

/// Takes the handle granted under slot `name`; a slot is taken once. `None` off the kernel, or
/// when nothing was granted under that name.
pub fn take(name: &str) -> Option<moruna_sys::Handle> {
    kernel()?;
    moruna_sys::start::take(name)
}

/// The names of the slots this process was granted, taken or not.
pub fn slots() -> Vec<String> {
    if kernel().is_none() {
        return Vec::new();
    }
    (0..moruna_sys::start::count())
        .filter_map(|i| moruna_sys::start::entry(i).map(|(_, _, name)| name.to_string()))
        .collect()
}
