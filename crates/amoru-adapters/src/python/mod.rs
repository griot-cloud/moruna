//! The Python kernel adapter (05-adapters sections d, e, f), behind the `python` feature.
//!
//! A plain Python callable, or an object with `setup` and `__call__`, becomes an
//! [`amoru_kernel::Kernel`]. The modules are the ones section l names: [`kernel`] for `apply`
//! (f.1, f.2) and the [`PyKernel`] surface, [`state`] for the per instance state (e.1, f.7),
//! [`cross`] for the export and import rules (e.2, e.3), [`copy`] for the boundary copy (f.3),
//! [`gil`] for GIL detection (f.4), [`fingerprint`] for the kernel fingerprint (e.4),
//! [`tensor_obj`] for the object that carries `__dlpack__`, and [`ctx`] for the `ctx` object
//! `setup` and `restore` receive.

pub mod copy;
pub mod cross;
pub mod ctx;
pub mod fingerprint;
pub mod gil;
pub mod kernel;
pub mod state;
pub mod tensor_obj;

pub use gil::{python_build_info, python_gil_enabled};
pub use kernel::{PyKernel, PyKernelSpec, PyKernelStats};
pub use state::PyState;
pub use tensor_obj::PyTensor;
