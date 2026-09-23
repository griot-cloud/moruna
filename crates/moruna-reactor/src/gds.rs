//! The GPUDirect Storage path (06 e.2, f.5, l): a hand-written minimal FFI over `libcufile`,
//! kept here because there is no `cufile` crate (preamble 6.2).
//!
//! The FFI is four calls: open the driver at `Reactor::new`, register a segment's descriptor
//! at `register_segment`, read from it straight into device memory in `copy`, and deregister
//! at `unregister_segment`. Nothing else of cuFile is used: v1 never writes through GDS
//! (f.5, the `Device -> Disk` row).
//!
//! Without the `gds` feature every item here is a refusal that keeps the call sites free of
//! `cfg`, so the `Disk -> Device` row of f.5 falls to the two-step the placement engine
//! issues (09 e.4).
//!
//! No GPU host exists (preamble E1), so nothing in this module has been run: the tests that
//! would exercise it (RE-T5, RE-T13's GDS half) are tagged "(reference host, E1)".

use std::os::fd::OwnedFd;
use std::path::Path;

use moruna_kernel::{MorunaError, Result};

/// A segment's cuFile handle, held in its registry entry (e.4) and deregistered with it.
pub(crate) struct Handle {
    #[cfg(feature = "gds")]
    raw: ffi::CUfileHandle,
}

// SAFETY: a cuFile handle is an opaque driver-side token used only through the driver's own
// thread-safe entry points; the reactor never dereferences it.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

/// Open the cuFile driver once at `Reactor::new`; `Ok(false)` means the driver is not usable
/// on this host, which is a fallback on an available path and an error on a guaranteed one
/// (RE-I3, decided by the caller).
pub(crate) fn driver_open() -> Result<bool> {
    #[cfg(feature = "gds")]
    {
        // SAFETY: `cuFileDriverOpen` takes no argument and returns a plain status struct.
        let status = unsafe { ffi::cuFileDriverOpen() };
        return Ok(status.err == 0);
    }
    #[cfg(not(feature = "gds"))]
    Ok(false)
}

/// Register a segment's descriptor with cuFile so `copy` can read from it into device memory.
pub(crate) fn register(_fd: &OwnedFd, _path: &Path) -> Result<Option<Handle>> {
    #[cfg(feature = "gds")]
    {
        use std::os::fd::AsRawFd;
        let mut raw: ffi::CUfileHandle = std::ptr::null_mut();
        let mut descr = ffi::CUfileDescr {
            handle_type: ffi::CU_FILE_HANDLE_TYPE_OPAQUE_FD,
            handle: ffi::CUfileHandleUnion {
                fd: _fd.as_raw_fd(),
            },
            fs_ops: std::ptr::null(),
        };
        // SAFETY: `raw` and `descr` are live for the call; `descr.handle.fd` is a descriptor
        // this process owns and keeps open for as long as the handle is registered (RE-I8).
        let status = unsafe { ffi::cuFileHandleRegister(&mut raw, &mut descr) };
        if status.err != 0 {
            return Err(MorunaError::Io {
                op: "register_segment",
                target: _path.display().to_string(),
                msg: format!("cuFileHandleRegister failed: {}", status.err),
            });
        }
        return Ok(Some(Handle { raw }));
    }
    #[cfg(not(feature = "gds"))]
    Ok(None)
}

/// Read `len` bytes of a registered segment at `file_offset` straight into device memory
/// (f.5, the `Disk -> Device` row). Returns the bytes read.
pub(crate) fn read(
    _handle: &Handle,
    _device_ptr: u64,
    _len: usize,
    _file_offset: u64,
) -> Result<usize> {
    #[cfg(feature = "gds")]
    {
        // SAFETY: the handle is registered and not yet deregistered, and the destination is a
        // device allocation of at least `len` bytes that the caller's `Buffer` keeps alive
        // until this operation resolves (RE-I1).
        let n = unsafe {
            ffi::cuFileRead(
                _handle.raw,
                _device_ptr as *mut std::ffi::c_void,
                _len,
                _file_offset as i64,
                0,
            )
        };
        if n < 0 {
            return Err(MorunaError::Io {
                op: "copy",
                target: "gds".into(),
                msg: format!("cuFileRead failed: {n}"),
            });
        }
        return Ok(n as usize);
    }
    #[cfg(not(feature = "gds"))]
    Err(MorunaError::Io {
        op: "copy",
        target: "gds".into(),
        msg: "gds is not built".into(),
    })
}

impl Drop for Handle {
    fn drop(&mut self) {
        #[cfg(feature = "gds")]
        // SAFETY: the handle was registered by `register` and is deregistered exactly once,
        // here, before the descriptor it was built over is closed.
        unsafe {
            ffi::cuFileHandleDeregister(self.raw)
        };
    }
}

#[cfg(feature = "gds")]
#[allow(non_camel_case_types, non_upper_case_globals)]
mod ffi {
    use std::ffi::{c_int, c_void};

    pub(super) type CUfileHandle = *mut c_void;
    pub(super) const CU_FILE_HANDLE_TYPE_OPAQUE_FD: c_int = 1;

    #[repr(C)]
    pub(super) struct CUfileError {
        pub(super) err: c_int,
        pub(super) cu_err: c_int,
    }

    #[repr(C)]
    pub(super) union CUfileHandleUnion {
        pub(super) fd: c_int,
        pub(super) handle: *mut c_void,
    }

    #[repr(C)]
    pub(super) struct CUfileDescr {
        pub(super) handle_type: c_int,
        pub(super) handle: CUfileHandleUnion,
        pub(super) fs_ops: *const c_void,
    }

    #[link(name = "cufile")]
    unsafe extern "C" {
        pub(super) fn cuFileDriverOpen() -> CUfileError;
        pub(super) fn cuFileHandleRegister(
            fh: *mut CUfileHandle,
            descr: *mut CUfileDescr,
        ) -> CUfileError;
        pub(super) fn cuFileHandleDeregister(fh: CUfileHandle);
        pub(super) fn cuFileRead(
            fh: CUfileHandle,
            dev_ptr_base: *mut c_void,
            size: usize,
            file_offset: i64,
            dev_ptr_offset: i64,
        ) -> isize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_the_feature_the_driver_is_absent_and_a_read_refuses() {
        assert_eq!(driver_open().expect("no error"), cfg!(feature = "gds"));
        if !cfg!(feature = "gds") {
            let handle = Handle {};
            let err = read(&handle, 0, 1, 0).expect_err("gds is not built");
            assert!(matches!(err, MorunaError::Io { op: "copy", .. }));
        }
    }
}
