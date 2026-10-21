//! What a Python kernel run in this process prints (MH 4.3 `log`): `sys.stdout` and
//! `sys.stderr` become streams that hand what is written on a worker thread to the stage that
//! worker works for, and `logging`'s root logger gets a handler that does the same with each
//! record and its level (`moruna._output`). What is written on any other thread goes to the
//! stream it went to before, which, under `moruna run` or `moruna serve`, is the run's as the
//! engine's.

use moruna_kernel::Level;
use moruna_kernel::output::{self, Stream};
use pyo3::prelude::*;

/// `sys.stdout` or `sys.stderr`, routed by the stage the writing thread works for.
#[pyclass(module = "moruna._core", name = "_OutputStream", frozen)]
pub struct OutputStream {
    stream: Stream,
    original: Py<PyAny>,
}

#[pymethods]
impl OutputStream {
    /// `fd` is 1 (standard output) or 2 (standard error); `original` the stream it replaces.
    #[new]
    fn new(fd: i32, original: Py<PyAny>) -> PyResult<OutputStream> {
        let stream = match fd {
            1 => Stream::Stdout,
            2 => Stream::Stderr,
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "a kernel's output is on descriptor 1 or 2, not {other}"
                )));
            }
        };
        Ok(OutputStream { stream, original })
    }

    /// Written as it comes: a worker's to its stage, anyone else's to the original stream.
    fn write(&self, py: Python<'_>, text: &str) -> PyResult<usize> {
        if output::write(self.stream, text.as_bytes()) {
            return Ok(text.chars().count());
        }
        self.original
            .bind(py)
            .call_method1("write", (text,))?
            .extract()
    }

    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        self.original.bind(py).call_method0("flush").map(|_| ())
    }

    fn isatty(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        true
    }

    /// Everything else a stream has is the original's.
    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        self.original.bind(py).getattr(name).map(Bound::unbind)
    }
}

/// `moruna._core._record(levelno, text)`: one logging record of the stage the calling thread
/// works for, at `levelno`'s level; `False`, and nothing kept, when it works for none.
#[pyfunction]
fn _record(levelno: i64, text: &str) -> bool {
    output::log(Level::of_python(levelno), text)
}

/// Register the stream and the record function on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<OutputStream>()?;
    m.add_function(wrap_pyfunction!(_record, m)?)?;
    Ok(())
}

/// Route this interpreter's kernel output by stage (`moruna._output.install`); the command
/// says why when it cannot, and the output then goes where it went.
pub fn install(py: Python<'_>) {
    if let Err(error) = py
        .import("moruna._output")
        .and_then(|m| m.call_method0("install"))
    {
        eprintln!(
            "moruna: a Python kernel's output cannot be told apart by stage ({error}); it is the \
             engine's"
        );
    }
}
