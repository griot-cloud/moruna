#!/usr/bin/env python3
"""Two explicit native IO acceptance boots; separate from the 15 engine-task boots.

Reuse proof.py's bounded owned lifecycle, production node/services and KE2's real
controlled TLS peer. Supply all ordinary proof.py immutable tool/input arguments
AFTER '--'; omit --out/--case/--cpus/--seconds (owned by this orchestrator).
No builds, downloads, production fallback or simulated service are performed.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--net-proof", type=Path, required=True)
    parser.add_argument("--cpus", type=int, choices=(1, 2, 4), default=1)
    parser.add_argument("--seconds", type=int, default=600)
    parser.add_argument("inputs", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    forwarded = args.inputs[1:] if args.inputs[:1] == ["--"] else args.inputs
    forbidden = ("--out", "--case", "--cpus", "--seconds", "--io-phase", "--io-home", "--io-net")
    if any(item.split("=", 1)[0] in forbidden for item in forwarded):
        parser.error("forward only ordinary immutable proof.py inputs")
    if len(forwarded) % 2 or any(not forwarded[i].startswith("--") for i in range(0, len(forwarded), 2)):
        parser.error("explicit --name VALUE input pairs required")
    options = dict(zip(forwarded[::2], forwarded[1::2]))
    if len(options) * 2 != len(forwarded):
        parser.error("duplicate forwarded input")
    for required in ("--kernel-elf", "--provision"):
        if required not in options:
            parser.error(f"missing {required}")
    if not 30 <= args.seconds <= 600 or not args.out.is_absolute() or args.out.exists():
        parser.error("new absolute owned output and per-boot bound 30..600 required")
    if not args.net_proof.is_absolute() or not args.net_proof.is_file():
        parser.error("explicit existing KE2 net-proof.py required")
    args.out.mkdir(mode=0o700)
    deadline = time.monotonic() + args.seconds * 2 + 120
    result = {"version": 1, "acceptance": "native-storage-network", "passed": False,
              "cleanup": False, "cpus": args.cpus, "boots": []}
    peer = None
    peer_log = None

    def remaining():
        left = deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("two-boot acceptance deadline expired")
        return left

    def command(values):
        with (args.out / "prepare.log").open("ab") as log:
            subprocess.run([str(v) for v in values], check=True, timeout=remaining(), stdout=log, stderr=subprocess.STDOUT)

    source_inputs = {"orchestrator": Path(__file__).resolve(), "net_proof": args.net_proof,
                     "proof": Path(__file__).with_name("proof.py").resolve()}
    source_hashes = {name: digest(path) for name, path in source_inputs.items()}
    result["source_inputs"] = source_hashes
    try:
        # Existing KE2 helper requires actual owned prebuilt kernel output. No VM
        # or compile is performed by prepare; its CA/key are controlled test inputs.
        shutil.copyfile(options["--kernel-elf"], args.out / "moruna-microkernel.elf")
        command(["python3", args.net_proof, "prepare", args.out, "--card", "net0"])
        peer_log = (args.out / "peer.log").open("wb")
        peer = subprocess.Popen(["python3", str(args.net_proof), "peer", str(args.out), "--connections", "2", "--timeout", "1200"],
                                stdin=subprocess.DEVNULL, stdout=peer_log, stderr=subprocess.STDOUT)
        ready_path = args.out / "ke2/peer-ready.json"
        while not ready_path.is_file():
            remaining()
            if peer.poll() is not None:
                raise RuntimeError("actual controlled TLS peer exited before binding")
            time.sleep(0.01)
        ready = json.loads(ready_path.read_text())
        home = args.out / "home.img"
        with home.open("xb") as image:
            image.truncate(64 * 1024 * 1024)
        root = args.out / "home.root"
        command([options["--provision"], "plan", "--output", root, "--capacity", 64 * 1024 * 1024,
                 "--alignment", 4096, "--log-bytes", 1024 * 1024, "--role", "single-node-home", "--workspace", "moruna-ci", "--bucket", "jobs"])
        command([options["--provision"], "format", "--image", home, "--root", root])
        owner = (home.stat().st_dev, home.stat().st_ino)
        root_hash = digest(root)
        result["initial_home_sha256"] = digest(home)
        result["root_sha256"] = root_hash
        proof = Path(__file__).with_name("proof.py").resolve()
        for phase in ("created", "existing"):
            # Regenerate only the explicit trusted UTC anchor/config using the same
            # CA/key and actual peer endpoint. NEVER provision/replace home again.
            command(["python3", args.net_proof, "prepare", args.out, "--card", "net0", "--port", ready["port"]])
            before = digest(home)
            command(["python3", proof, *forwarded, "--out", args.out / phase, "--case", "1", "--cpus", args.cpus,
                     "--seconds", args.seconds, "--io-phase", phase, "--io-home", home, "--io-net", args.out / "ke2"])
            facts = json.loads((args.out / phase / "result.json").read_text())
            if not facts.get("passed") or not facts.get("cleanup") or facts.get("acceptance") != "native-storage-network":
                raise RuntimeError("native boot did not establish real IO facts and owned cleanup")
            # Exact bytes of canonical Response::Head with constant fixture id.
            # Hex is transport only: no second metadata parser/hash/authority.
            prefix = "KE3 native storage receipt: "
            receipts = [line.split(prefix, 1)[1].strip() for line in
                        (args.out / phase / "serial.log").read_text(errors="strict").splitlines()
                        if prefix in line]
            if len(receipts) != 1 or not receipts[0] or len(receipts[0]) > 4096 or len(receipts[0]) % 2 or any(c not in "0123456789abcdef" for c in receipts[0]):
                raise RuntimeError("one exact bounded canonical metadata receipt required")
            receipt = receipts[0]
            if result["boots"] and receipt != result["boots"][0]["metadata_receipt"]:
                raise RuntimeError("acknowledged immutable metadata/version changed across reboot")
            after = digest(home)
            if (home.stat().st_dev, home.stat().st_ino) != owner or digest(root) != root_hash:
                raise RuntimeError("same actual device image/root ownership changed")
            if phase == "created" and before == after:
                raise RuntimeError("write boot did not change actual provisioned home bytes")
            if phase == "existing" and before != after:
                raise RuntimeError("recovery-only boot unexpectedly mutated home bytes")
            result["boots"].append({"phase": phase, "run_id": facts["run_id"], "before_sha256": before,
                                    "after_sha256": after, "metadata_receipt": receipt, "result_sha256": digest(args.out / phase / "result.json")})
        if result["boots"][0]["run_id"] == result["boots"][1]["run_id"] or result["boots"][0]["after_sha256"] != result["boots"][1]["before_sha256"]:
            raise RuntimeError("distinct canonical boot/run identity or exact disk continuity missing")
        if peer.wait(timeout=min(30, remaining())) != 0:
            raise RuntimeError("controlled peer did not verify both genuine TLS exchanges")
        if {name: digest(path) for name, path in source_inputs.items()} != source_hashes:
            raise RuntimeError("acceptance source inputs changed during execution")
        result["passed"] = True
    except Exception as error:
        result["error"] = str(error)
    finally:
        try:
            if peer is not None:
                if peer.poll() is None:
                    peer.kill()
                peer.wait(timeout=10)
            result["cleanup"] = True
        except (OSError, subprocess.TimeoutExpired):
            result["passed"] = False
        if peer_log is not None:
            peer_log.close()
        result["peer"] = None if peer is None else {"pid": peer.pid, "returncode": peer.returncode}
        result["passed"] = bool(result["passed"] and result["cleanup"])
        (args.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    if not result["passed"]:
        raise SystemExit(f"native IO acceptance failed; actual facts: {args.out / 'result.json'}")


if __name__ == "__main__":
    main()
