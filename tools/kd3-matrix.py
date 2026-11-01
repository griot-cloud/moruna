#!/usr/bin/env python3
"""Strict comparison of ACTUAL KD3 result JSON files; this does not run a VM or
manufacture results. `--requirements` lists every required real invocation.

kd3-matrix.py MATRIX --requirements
kd3-matrix.py MATRIX RESULT_DIRECTORY
RESULT_DIRECTORY must contain only raw JSON outputs from actual fixture invocations;
keep engine logs elsewhere. Every required adapter/case/phase is mandatory;
CPU-profile boot coverage is separately checked by the real boot gate.
"""
import json
import pathlib
import sys


def requirements(matrix):
    required = []
    for case in matrix["cases"]:
        pairs = [("execute", "rows")]
        if case["runner"] == "document" and case["expect"] == "rows":
            pairs.append(("execute", "unknown-setting"))
            if case["role"] == "sink":
                pairs.append(("readback", "rows"))
        if case["runner"] == "python-library":
            pairs.append(("execute", "adapter-refusal"))
        for adapter in ("linux", "native"):
            for phase, mode in pairs:
                required.append((case["id"], adapter, phase, mode))
    return required


def main():
    matrix = json.loads(pathlib.Path(sys.argv[1]).read_text())
    required = requirements(matrix)
    if sys.argv[2] == "--requirements":
        print(json.dumps(required, indent=2))
        return
    records = {}
    for path in sorted(pathlib.Path(sys.argv[2]).glob("*.json")):
        record = json.loads(path.read_text())
        key = tuple(record[name] for name in ("case", "adapter", "phase", "mode"))
        if key not in required or key in records:
            raise ValueError(f"unexpected/duplicate actual result {path}: {key}")
        records[key] = record
    missing = set(required) - records.keys()
    if missing:
        raise ValueError(f"missing {len(missing)} REQUIRED actual invocations: {sorted(missing)}")
    expected = sorted(matrix["rows"])
    for case in matrix["cases"]:
        keys = [(phase, mode) for cid, adapter, phase, mode in required if cid == case["id"] and adapter == "linux"]
        for phase, mode in keys:
            pair = [records[(case["id"], adapter, phase, mode)] for adapter in ("linux", "native")]
            refused = mode in ("unknown-setting", "adapter-refusal") or case["expect"] == "strict-import-refusal"
            if refused:
                if any(value["outcome"] != "refused" or not value.get("diagnostic") for value in pair):
                    raise ValueError(f"missing actual refusal: {case['id']} {mode}")
                # Same exact logical paths in both adapter fixtures: do not normalize away
                # permission, schema, runtime or native transport differences.
                if pair[0]["diagnostic"] != pair[1]["diagnostic"]:
                    raise ValueError(f"refusals differ: {case['id']} {mode}: {pair}")
            else:
                for value in pair:
                    if case["runner"] == "document" and (not isinstance(value.get("outside_after"), list) or any(value["outside_after"])):
                        raise ValueError(f"actual task reservation did not retire: {case['id']}: {value}")
                    if value["outcome"] != "rows" or sorted(value["rows"]) != expected or value["finish"] != 1 or value["committed"] != len(expected):
                        raise ValueError(f"real rows/commit/finish mismatch: {case['id']} {phase}: {value}")
    for case in matrix["cases"]:
        if case["runner"] == "document" and case["role"] == "sink" and case["expect"] == "rows":
            for adapter in ("linux", "native"):
                produced = records[(case["id"], adapter, "execute", "rows")]
                readback = records[(case["id"], adapter, "readback", "rows")]
                if not produced.get("files") or readback.get("producer") != produced:
                    raise ValueError(f"readback is not tied to exact actual producer receipt: {case['id']} {adapter}")
    print(f"KD3: {len(records)} actual invocation results agree across all {len(matrix['cases'])} required cases; every sink read back")


if __name__ == "__main__":
    main()
