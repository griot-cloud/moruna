# Python API reference

Moruna requires Python 3.14 or later and `pyarrow>=17`. The names below are available from `import moruna`, except the functions under `moruna.std`. `moruna.__version__: str` is the installed package version.

| Reference | What you can look up |
| --- | --- |
| [`moruna.run`](python-run.md) | All run arguments, defaults, return value and failures. |
| [Kernels](python-kernels.md) | `moruna.kernel`, `KernelSpec`, `moruna.polars`, state methods and declarations. |
| [Sources](python-sources.md) | Built-in constructors, `Source` methods and `Split` attributes. |
| [Sinks](python-sinks.md) | Built-in constructors and `Sink` methods. |
| [Standard kernels](python-standard-kernels.md) | Every `moruna.std` function and `StdKernel` attributes. |
| [Results and errors](python-results.md) | `RunReport`, `inspect_host`, and the exception hierarchy. |

For longer examples, see [Running jobs from Python](python.md), [Writing kernels](kernels.md), and [Custom sources and sinks](sources-and-sinks.md).

```{toctree}
:hidden:
:maxdepth: 1

python-run
python-kernels
python-sources
python-sinks
python-standard-kernels
python-results
```
