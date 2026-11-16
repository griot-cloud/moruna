"""The PM's time split (control 5) from a Moruna run report, for 0.6.6 (runner.py) and 0.7
(driver.py's moruna-vm), as far as the report has it.

0.6.6's report gives each task its busy time and the function's waits on its input and output:
the split's function is the function tasks' busy time, its waits theirs, read and decode the
source tasks' busy time, encode and write the sink tasks'. It does not split decode from read or
encode from write, and a task without a busy time counts zero. What 0.7's report adds (stream C's
`Sampler` on the run's and tasks' accounts) is read from the same fields.
"""


def split(d):
    tasks = d.get("tasks", []) or []

    def busy(role):
        return sum((t.get("busy_ms") or 0) for t in tasks if t.get("role") == role) / 1e3

    fns = [t for t in tasks if t.get("role") == "function"]
    return {"split": {"read_decode_s": busy("source"), "function_s": busy("function"),
                      "encode_write_s": busy("sink"),
                      "wait_s": sum((t.get("input_wait_ms") or 0) + (t.get("output_wait_ms") or 0)
                                    for t in fns) / 1e3},
            "exit": d.get("exit"), "peak_anon_bytes": d.get("peak_anon_bytes")}
