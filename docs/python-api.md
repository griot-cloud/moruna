# Python API reference

Moruna requires Python 3.14 or later and `pyarrow>=17`. The names below are available from `import moruna`, except the functions under `moruna.std`. `moruna.__version__: str` is the installed package version.

| Reference | What you can look up |
| --- | --- |
| [Jobs and `moruna.run`](python-run.md) | `moruna.Job`, its spec, `Task`, `Source`, `Function`, `Sink`, needs and the resolver, the writer, and every run argument. |
| [Functions](python-functions.md) | `moruna.function`, `moruna.polars`, `Function.build`, state methods and declarations. |
| [Sources](python-sources.md) | Built-in constructors, `Source` methods and `Split` attributes. |
| [Sinks](python-sinks.md) | Built-in constructors and `Sink` methods. |
| [Standard functions](python-standard-functions.md) | Every `moruna.std` function. |
| [Results and errors](python-results.md) | `RunReport`, `inspect_host`, `check`, and the exception hierarchy. |

For longer examples, see [Running jobs from Python](python.md), [Writing functions](functions.md), and [Custom sources and sinks](sources-and-sinks.md).

```{toctree}
:hidden:
:maxdepth: 1

python-run
python-functions
python-sources
python-sinks
python-standard-functions
python-results
```
