"""The gate's jobs as 0.7 job documents (MJS 1, docs/job-document.md): a Parquet source, the job's
function as a native task, a Parquet sink.

A native task is named the way a job names any program of its own: an `image` function whose
image holds the task (images/gate/tasks, gate-JOB, a static program of the kernel's user target)
and whose `command` runs it, with `plane: true`, the command making the data plane's calls itself,
and `abi: "moruna"`: the engine checks the program is of the kernel's user target and starts it
itself, a process of the kernel with its plane attached, never through the Linux personality
(stream P).

The inputs are named as `moruna run` takes them; on the data OS each becomes a `disk://NAME`
device (crates/moruna-micro/src/devices.rs). With disk=True the document names the devices,
`disk://` plus a name made from the file's, and a file beside it (JOB.inputs.json) maps each
name to its file, for a launcher that is given both; the document itself has no field for it.

    gate_jobs.py WORKLOAD OUT_DIR JOB.json FILE...   # writes one document
"""
import json
import os
import re
import sys

SINK = {"compression": "zstd", "row_group_bytes": 128 << 20, "file_bytes": 1 << 30}
JOBS = ("score", "occupancy", "enrich", "audit", "ov_shape", "ov_attrs", "fw_minhash")


def disk_name(path: str, taken: set) -> str:
    """A `disk://` name for a file: `[a-z0-9._-]{1,24}`, unique in the job."""
    base = re.sub(r"[^a-z0-9._-]", "-", os.path.basename(path).lower().removesuffix(".parquet"))
    base = base.removeprefix("yellow_tripdata_").strip("-") or "input"
    name, n = base[:24], 1
    while name in taken:
        suffix = f"-{n}"
        name, n = base[:24 - len(suffix)] + suffix, n + 1
    taken.add(name)
    return name


def document(workload: str, files, out: str, image: dict, cpus: int = 8, mem_gib: float = 24,
             disk: bool = False) -> dict:
    if workload not in JOBS:
        raise ValueError(f"no gate job {workload!r}")
    doc = {"moruna_spec": 1}
    if disk:
        doc["source"] = {"kind": "parquet", "url": [f"disk://{n}" for n in inputs(files)]}
    else:
        doc["source"] = {"kind": "parquet", "url": list(files)}
    doc["kernels"] = [{
        "kind": "image",
        "image": {"build_key": image.get("build_key"), "digest": image.get("digest"),
                  "location": image.get("location")},
        "command": [f"/gate-{workload}"],
        "plane": True,
        "abi": "moruna",
    }]
    doc["sink"] = {"kind": "parquet", "url": out, "options": dict(SINK)}
    doc["budget"] = {"memory_bytes": int(mem_gib * (1 << 30)), "cpu": cpus}
    return doc


def inputs(files) -> dict:
    """The `disk://` names of `files`, as `document(..., disk=True)` names them."""
    taken = set()
    return {disk_name(f, taken): f for f in files}


def write(path: str, workload: str, files, out: str, image: dict, **kw) -> dict:
    doc = document(workload, files, out, image, **kw)
    with open(path, "w") as f:
        json.dump(doc, f, indent=1)
    if kw.get("disk"):
        with open(path.removesuffix(".json") + ".inputs.json", "w") as f:
            json.dump(inputs(files), f, indent=1)
    return doc


if __name__ == "__main__":
    wl, out, path, *files = sys.argv[1:]
    image = {"digest": os.environ.get("GATE_IMAGE_DIGEST"), "location": os.environ.get("GATE_IMAGE_LAYOUT")}
    write(path, wl, files, out, image)
