"""The Python half of the Amoru benchmark kernels (preamble section 6.5).

Preamble 6.5 asks for one kernel of the six that is not Rust: ``wide-intermediate``
(NumPy, amplification about 20, releases the GIL). Its body is here because the
thing the suite needs to measure is a Python extension's cost and a Python
extension's GIL behaviour, which a Rust function wearing a Python name would not
give.

Nothing in this package imports the runtime. The runtime's Python adapter
(component 5) does not exist yet, so the Rust side of this kernel
(``bench/src/kernels/wide_intermediate.rs``) declares the kernel and refuses to
run it rather than shipping a binding that is not the one component 5 will build.

Run the tests from ``bench/python``::

    uv run --python 3.14 --with numpy --with pyarrow --with pytest pytest

and again with ``--python 3.14t`` for the free threaded interpreter.
"""

__version__ = "0.1.0"
__all__ = ["__version__", "wide_intermediate"]

from amoru_bench_kernels import wide_intermediate
