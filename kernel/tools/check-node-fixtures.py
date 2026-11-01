#!/usr/bin/env python3
"""Require actual successor-node evidence for ALL five KD2 cases on1/2/4 CPUs.
This only validates real runner artifacts; it does not run a VM or synthesize them.
"""
import hashlib
import json
from pathlib import Path
import sys

NAMES = ("rows", "a task that fails", "a refused record", "cancel", "an image nobody named")


def main():
    root = Path(sys.argv[1])
    if not root.is_absolute():
        raise ValueError("explicit absolute node evidence root required")
    baseline = None
    for cpus in (1, 2, 4):
        for case, name in enumerate(NAMES, 1):
            out = root / f"cpu{cpus}" / f"case{case}"
            result = json.loads((out / "result.json").read_text())
            if (result.get("version"), result.get("cpus"), result.get("case"), result.get("passed"), result.get("cleanup"), result.get("mode")) != (1, cpus, case, True, True, "explicit-test-tcg"):
                raise ValueError(f"incomplete/failed actual node case: {out}")
            if result.get("transport") == "native-sdk":
                sdk = result.get("sdk", {})
                actual = json.loads((out / "sdk/result.json").read_text())
                terminal = json.loads((out / "sdk/terminal.json").read_text())
                key = json.loads((out / "sdk/prepared-key.json").read_text())
                config = json.loads((out / "sdk-config.json").read_text())
                witness = out / "state" / f"{key['run_id']}.launch"
                phase = terminal.get("record", {}).get("phase")
                if (result.get("cleanup_method") != "sdk-owned-exited" or actual.get("passed") is not True
                        or (actual.get("case"), actual.get("cpus"), actual.get("mode")) != (case, cpus, "explicit-test-tcg")
                        or actual.get("key") != key or sdk.get("key") != key or key.get("run_id") != result.get("run_id")
                        or key.get("spec_digest") != result.get("engine_spec_digest")
                        or key.get("node_boot") != config.get("node_boot") or key.get("attempt") != config.get("attempt")
                        or actual.get("terminal") != terminal.get("record") or terminal.get("record", {}).get("key") != key
                        or not isinstance(phase, dict) or set(phase) != {"Exited"} or type(phase["Exited"]) is not int
                        or not terminal.get("probe", "").startswith("Exited(")
                        or not witness.is_file() or witness.is_symlink() or witness.stat().st_size == 0
                        or hashlib.sha256(witness.read_bytes()).hexdigest() != sdk.get("witness_sha256")
                        or hashlib.sha256((out / "sdk/result.json").read_bytes()).hexdigest() != sdk.get("result_sha256")
                        or hashlib.sha256((out / "sdk/terminal.json").read_bytes()).hexdigest() != sdk.get("terminal_sha256")
                        or hashlib.sha256((out / "sdk-descriptor.json").read_bytes()).hexdigest() != sdk.get("descriptor_sha256")):
                    raise ValueError(f"uncertain/failed actual SDK lifecycle/key/witness: {out}")
            else:
                status = (result.get("qemu_exit"), result.get("cleanup_method"))
                if status not in ((33, "guest-normal33"), (0, "qmp-quit")):
                    raise ValueError(f"uncertain/failed actual QEMU lifecycle: {out}: {status}")
            inputs = result.get("inputs")
            if not isinstance(inputs, dict) or not inputs or any(not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdef" for c in value) for value in inputs.values()):
                raise ValueError(f"immutable input hashes absent/invalid: {out}")
            actual = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
            if result.get("inputs_sha256") != actual:
                raise ValueError(f"actual input census mismatch: {out}")
            if baseline is None:
                baseline = actual
            if baseline != actual:
                raise ValueError(f"native profiles were tested with different immutable inputs: {out}")
            serial = (out / "serial.log").read_text(errors="replace")
            peer = (out / "peer.log").read_text(errors="replace")
            if f"kk: {cpus} CPUs online" not in serial or "panic" in serial.lower() or "FAIL:" in serial or "FAIL:" in peer:
                raise ValueError(f"actual CPU boot or peer failed: {out}")
            prefix = f"embedder: ok: run {case} ({name}):"
            if sum(line.startswith(prefix) for line in peer.splitlines()) != 1:
                raise ValueError(f"original strict case verdict missing/duplicated: {out}")
            # Preserve the predecessor's real record-check fault witness as well as
            # case3's typed failed-task/refusal assertions in the canonical peer.
            if case == 3 and "faulted: its record on slot 1 was refused by the engine's check:" not in serial:
                raise ValueError(f"original record-check fault witness missing: {out}")
    print("native node fixtures: all15 real case/profile results passed with one immutable input census and confirmed cleanup")


if __name__ == "__main__":
    main()
