# Running jobs from the command line

The `moruna` command runs a job described in a JSON file, a **job document**. The document holds everything `moruna.run` takes as arguments: the source, the kernels, the sink, the budget and the other options. A scheduler, a container entry point or a virtual machine can run a job this way without a Python program written for it.

The command is installed with the Python package. A job from a document and the same job from Python run identically.

## A job document

This document runs the quickstart's scoring kernel over the reviews dataset, after removing reviews with no text:

```json
{
  "moruna_spec": 1,
  "source": {
    "kind": "parquet",
    "url": "reviews/",
    "options": {"columns": ["review_id", "text"]}
  },
  "kernels": [
    {"kind": "std", "name": "filter", "args": {"expr": "is_not_null(text)"}},
    {"kind": "python", "module": "kernels.py", "callable": "score"}
  ],
  "sink": {"kind": "parquet", "url": "reviews-scored/"},
  "budget": {"memory_bytes": 1073741824},
  "report": {"file": "score-report.json"}
}
```

`moruna_spec` is the version of the document format and is always `1`. A Python kernel is named by its `module`, a module name or a path to a `.py` file, and the `callable` inside it. A standard kernel is named by `name`, with its arguments in `args`. Paths in the document are relative to the directory you run the command from.

Fields you leave out take the same defaults as `moruna.run`. Every field is described in the [job document reference](job-document.md).

## Running a document

```text
moruna run job.json
moruna: exit 0; report: score-report.json
```

The command prints one line when the job ends, with its exit code and where the report was written. It always writes the report to a file: to `report.file` if the document sets it, otherwise to `moruna-<run id>.report.json` in the staging directory or the current directory. The exit code says how the job ended:

| Code | Meaning |
| --- | --- |
| 0 | The job completed. |
| 1 | The job failed reading or writing data. |
| 2 | The document was refused; the message names the field. |
| 3 | The budget cannot hold the job. |
| 4 | A kernel failed and the error policy stopped the job. |
| 5 | Resuming was refused. |
| 130 | The job was cancelled. |

## Strict mode

Some settings can also come from environment variables, such as `MORUNA_BUDGET` for the budget. That is convenient on your own machine, but on a server it means the same document can run differently depending on where it runs. With `--strict`, `moruna run` refuses a document that would take any setting from the environment, and names it:

```text
moruna run job.json --strict
moruna: exit 2: spec refused: budget.memory_bytes: strict mode resolves no field from the environment, and this document would resolve budget.memory_bytes from MORUNA_BUDGET; set the field in the document or unset the variable
```

Use `--strict` wherever documents are run by other systems.

## Letting a host control the job

`moruna serve` waits for a job document on a socket, runs it, and reports back on the same connection. It is for a host program, such as a job scheduler, that starts Moruna and wants to follow the job while it runs:

```text
moruna serve --listen unix:///run/moruna.sock
```

The host connects, sends the document, and receives newline-separated JSON messages: a greeting with the limits Moruna found, a progress message at least every two seconds, a message when the machine's limits change, the full report and finally the exit code. The host can send `cancel` to stop the job, or `checkpoint` to have it save its progress immediately, for example before the host shuts the machine down. `moruna serve` runs one job and exits, and it is strict by default. The messages are listed in the [command reference](cli.md#the-host-protocol).

## Checking kernels

`moruna check` loads a module or file and checks every kernel in it against its declared schemas, without a source, a sink or any real data:

```text
moruna check kernels.py
```

See [Checking a kernel](kernels.md#checking-a-kernel) for what it tests and what it reports.

Next: [Where Moruna runs](hosting.md).
