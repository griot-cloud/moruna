# Contributing

Moruna is built in Rust, with a Python package on top. Changes to how data is read, sized, held in memory or written belong in the Rust crates; changes to the Python surface belong in the `moruna-py` crate and the `python/` package.

## Build and check

Use the Rust toolchain pinned in `rust-toolchain.toml`. From the repository root, install the hooks once:

```bash
tools/hooks/install.sh
```

Before each commit, a hook runs the quick checks in a few seconds: formatting, lints and a compile of every test. Before each push, it runs the full gate that the continuous integration runs, which also runs every test and requires at least 90% line coverage in each crate. You can run either yourself with `tools/quality/check.sh --fast` or `tools/quality/check.sh`. A change to what Moruna does should include a test that shows it, on any machine: assert a property, such as the peak memory staying under the budget, rather than a figure measured on your own computer.

## Build the Python package

From the repository root, with a free-threaded Python 3.14 virtual environment active:

```bash
python -m pip install maturin pyarrow pytest
maturin develop --release
python -m pytest -q python/tests
```

## Build the documentation

From the repository root, with a Python environment active:

```bash
python -m pip install -r docs/requirements.txt
sphinx-build -W --keep-going -b html docs docs/_build/html
python -m http.server 8765 --bind 127.0.0.1 --directory docs/_build/html
```

Open `http://127.0.0.1:8765` to review the site. Treat warnings as failures, and run changed examples against a built Moruna before submitting them. The site's stylesheet, `docs/_static/griot.css`, is shared with the parcel and peQL sites; change all three together.

The repository's [contributor guide](https://github.com/griot-cloud/moruna/blob/main/CONTRIBUTING.md) covers pull requests, releases and signing off commits. Report problems with the job, a small sample of the data, the run report's `to_json()` output and the full error message.
