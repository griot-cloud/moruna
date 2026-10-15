//! A coverage build's profile of a process that ends through `_exit` (17 l): a child forked from
//! the template, or a test's forked child, skips the exit handler that writes the profile, and a
//! confined child cannot open the profile's path. `open` makes a file of its own beside the one
//! `LLVM_PROFILE_FILE` names, before confinement; `write` puts the counters into it just before
//! the process ends. Outside a coverage build both are nothing.

#[cfg(coverage)]
mod imp {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    unsafe extern "C" {
        fn __llvm_profile_get_size_for_buffer() -> u64;
        fn __llvm_profile_write_buffer(buffer: *mut core::ffi::c_char) -> core::ffi::c_int;
    }

    static FILE: Mutex<Option<File>> = Mutex::new(None);

    /// Create this process's profile file beside `LLVM_PROFILE_FILE`'s, before confinement.
    pub fn open() {
        let Some(pattern) = std::env::var_os("LLVM_PROFILE_FILE") else {
            return;
        };
        let dir = Path::new(&pattern).parent().unwrap_or(Path::new("."));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "moruna-sandbox-{}-{nanos}.profraw",
            std::process::id()
        ));
        match File::create(&path) {
            Ok(file) => *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(file),
            Err(e) => eprintln!("coverage: {}: {e}", path.display()),
        }
    }

    /// Write the profile into the file `open` made, before the process ends.
    pub fn write() {
        let Some(mut file) = FILE.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        // SAFETY: the profile runtime linked into every coverage build; the buffer is the size
        // it asked for.
        let written = unsafe {
            let mut buffer = vec![0u8; __llvm_profile_get_size_for_buffer() as usize];
            (__llvm_profile_write_buffer(buffer.as_mut_ptr().cast()) == 0).then_some(buffer)
        };
        match written {
            Some(buffer) => {
                if let Err(e) = file.write_all(&buffer) {
                    eprintln!("coverage: the profile was not written: {e}");
                }
            }
            None => eprintln!("coverage: the profile runtime wrote no buffer"),
        }
    }
}

#[cfg(coverage)]
pub use imp::{open, write};

/// Nothing outside a coverage build.
#[cfg(not(coverage))]
pub fn open() {}

/// Nothing outside a coverage build.
#[cfg(not(coverage))]
pub fn write() {}
