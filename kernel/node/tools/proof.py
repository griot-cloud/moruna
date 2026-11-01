#!/usr/bin/env python3
"""One bounded genuine node CI case. Caller owns the shared execution reservation.

Uses prebuilt production services/codecs and explicit existing immutable inputs.
TCG is intentional test harness ONLY, never the production KVM launcher fallback.
No compilation/download, fake service, permission parser or successful boot claim.
"""
import argparse
import hashlib
import json
import os
import uuid
from pathlib import Path
import socket
import subprocess
import time

MIB = 1024 * 1024
RAM = 3 * 1024 * MIB
PORT = 5100


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(MIB), b""):
            h.update(block)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cpus", type=int, choices=(1, 2, 4), required=True)
    parser.add_argument("--case", type=int, choices=range(1, 6), required=True)
    parser.add_argument("--seconds", type=int, default=180)
    for name in ("qemu", "backend", "peer", "fixture-emitter", "validator", "image-builder", "provision", "kernel-elf", "user-bins", "image-record", "linux-layers", "linux-digest", "tls-roots", "layer-fixture-1", "layer-fixture-2", "engine-layer-dir"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--sdk-proof", type=Path, help="Explicit nondefault Rust native SDK TCG proof executable; sole VM owner")
    parser.add_argument("--io-phase", choices=("created", "existing"))
    parser.add_argument("--io-home", type=Path)
    parser.add_argument("--io-net", type=Path)
    args = parser.parse_args()
    if any((args.io_phase is not None, args.io_home is not None, args.io_net is not None)) != all((args.io_phase is not None, args.io_home is not None, args.io_net is not None)):
        parser.error("IO fixture requires phase, same provisioned home, and existing net-proof inputs together")
    if args.sdk_proof and args.io_phase:
        parser.error("SDK engine-case proof cannot stand in for the separate native storage/network IO actor")
    if not 10 <= args.seconds <= 600:
        parser.error("explicit per-case bound must be 10..600 seconds")
    out = args.out.absolute()
    if out.exists():
        parser.error("output must be a new owned directory")
    for name, value in vars(args).items():
        if isinstance(value, Path) and name != "out":
            if not value.is_absolute() or not value.exists() or any(c in str(value) for c in ",\n\r"):
                parser.error(f"explicit input unavailable/unsafe for QEMU: {name}")
    if len(str(out / "vhost.sock").encode()) >= 100:
        parser.error("owned output path too long for Unix sockets")
    layers = []
    index = 1
    while (args.engine_layer_dir / f"stack-{index}.img").is_file():
        layers.append(args.engine_layer_dir / f"stack-{index}.img")
        index += 1
    if not layers or len(layers) > 16:
        parser.error("explicit bounded engine layer stack required")
    out.mkdir(mode=0o700)
    deadline = time.monotonic() + args.seconds
    record = {"version": 1, "cpus": args.cpus, "case": args.case, "passed": False, "cleanup": False}
    children = []
    logs = []
    sdk_cleanup = not bool(args.sdk_proof)

    def remaining():
        left = deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("node CI case deadline expired")
        return left

    def command(argv):
        with (out / "prepare.log").open("ab") as log:
            subprocess.run([str(value) for value in argv], check=True, timeout=remaining(), stdout=log, stderr=subprocess.STDOUT)

    def spawn(argv, name):
        log = (out / name).open("wb")
        logs.append(log)
        child = subprocess.Popen([str(value) for value in argv], stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        children.append(child)
        return child

    input_files = {}
    try:
        tools = Path(__file__).resolve().parent
        description = tools.parent / "standalone.image"
        input_files = {name: getattr(args, name) for name in
                         ("qemu", "backend", "kernel_elf", "peer", "fixture_emitter", "validator", "image_builder", "provision", "image_record", "linux_layers", "linux_digest", "tls_roots", "layer_fixture_1", "layer_fixture_2")}
        if args.sdk_proof:
            input_files["sdk_proof"] = args.sdk_proof
        for name in ("node-supervisor", "node-admission", "devices", "imagefs", "linux", "netstack", "engine", "storage", "virtio-blk", "virtio-vsock", "virtio-net", "nvme", "gve"):
            input_files[f"elf:{name}"] = args.user_bins / name
        if args.io_phase:
            for name in ("node-storage-proof", "netstack-test"):
                input_files[f"elf:{name}"] = args.user_bins / name
            input_files["io_roots"] = args.io_net / "roots"
            input_files["io_network"] = args.io_net / "config"
            input_files["io_peer_endpoint"] = args.io_net / "peer-ready.json"
        for index, layer in enumerate(layers, 1):
            input_files[f"engine_layer:{index}"] = layer
        input_files["proof_source"] = Path(__file__).resolve()
        input_files["configure_source"] = tools / "configure.py"
        input_files["assembly_source"] = description
        common_inputs = {name: digest(path) for name, path in input_files.items()}
        record["inputs_sha256"] = hashlib.sha256(json.dumps(common_inputs, sort_keys=True).encode()).hexdigest()
        record["inputs"] = common_inputs
        run = (f"c200{args.cpus:02x}{1 if args.io_phase == 'created' else 2:02x}" if args.io_phase else f"c100{args.cpus:02x}{args.case:02x}") + "00" * 12
        # ONE canonical case definition remains in kd3-peer, including assigned id.
        command([args.peer, "--emit-job", args.linux_digest, args.case, run, out / "job.json"])
        command([args.fixture_emitter, out / "job.json", out / "canonical", *(["--storage-write-proof"] if args.io_phase else [])])
        identity = json.loads((out / "canonical/identity.json").read_text())
        if identity.get("version") != 1 or identity.get("run_id") != run:
            raise ValueError("canonical emitted job identity disagrees with assigned run")
        engine_spec_digest = identity.get("engine_spec_digest")
        if not isinstance(engine_spec_digest, str) or not engine_spec_digest.startswith("sha256:") or len(engine_spec_digest) != 71 or any(c not in "0123456789abcdef" for c in engine_spec_digest[7:]):
            raise ValueError("canonical emitter did not supply an engine spec digest")
        profile = {"version": 1, "node_memory_bytes": RAM, "cpus": args.cpus, "agent_port": PORT,
                   "job_memory_bytes": 1024 * MIB, "job_account_bytes": 1408 * MIB, "scratch_bytes": 256 * MIB,
                   "engine_program": "/moruna/bin/moruna-engine", "storage_provider": "single-node-home",
                   "service_accounts": {"devices": 128 * MIB, "imagefs": 256 * MIB, "linux": 256 * MIB,
                                        "network": 64 * MIB, "admission": 1536 * MIB, "storage": 128 * MIB}}
        (out / "profile.json").write_text(json.dumps(profile) + "\n")
        (out / "storage.conf").write_text("moruna-storage 1\ntimeout-ns 5000000000\nbody-limit 1048576\nsession-limit 4\nprovider single-node-home\nblock-service nvme0\nalignment 4096\n")
        (out / "network.conf").write_text(f"moruna-netstack 1\nsetting tls-unix-seconds {int(time.time())}\nsetting tls-time-valid-ms 3600000\ngrant listen-engine rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8\n")
        command(["python3", tools / "configure.py", "--profile", out / "profile.json", "--out", out / "assets",
                 "--image-record", args.image_record, "--layers", args.linux_layers, "--network", out / "network.conf",
                 "--tls-roots", args.io_net / "roots" if args.io_phase else args.tls_roots, "--engine-bindings", out / "canonical/engine.bindings",
                 "--storage-config", out / "storage.conf", "--storage-grants", out / "canonical/storage.grants",
                 "--storage-admission", out / "canonical/storage.admission"])
        if args.io_phase:
            # Explicit fixture assembly uses the unmodified production node parser,
            # supervisor, authority and storage owner, not a substitute service.
            text = description.read_text()
            if text.count("handle program:engine image.engine") != 1:
                raise ValueError("exact production initial ELF assembly seam required")
            text = text.replace("handle program:engine image.engine", "handle program:node-storage-proof image.engine")
            description = out / "io.image"
            description.write_text(text)
        command([args.validator, description, out / "assets", RAM, args.cpus, PORT])
        resources = json.loads((out / "assets/node-resources.json").read_text())
        expected_resources = {key: profile[key] for key in
                              ("cpus", "node_memory_bytes", "job_memory_bytes", "job_account_bytes", "scratch_bytes")}
        if resources != expected_resources:
            raise ValueError("validated actual fixed profile disagrees with QEMU")
        if args.io_phase:
            # A real one-shot test actor must not masquerade as a resident critical
            # node service. Existing image-builder links transfer its real listener.
            # The single-supervisor validator above checks the unchanged node
            # resource/core contract. These explicit auxiliary test links/configs
            # are then parsed by the SAME image/node/net parsers at assembly/boot.
            with (out / "assets/node.conf").open("a") as config:
                config.write("grant network listen-test network-proof\n")
            network = (args.io_net / "config").read_bytes() + b"\ngrant listen-engine rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=1\n"
            (out / "network.conf").write_bytes(network)
            (out / "assets/network.conf").write_bytes(network)
            with description.open("a") as image:
                image.write("\nprocess netstack-test\n  handle console console\n  expect KE2 user DHCP: ok:\n  expect KE2 user DNS: ok:\n  expect KE2 user TLS: ok:\n  expect KE2 conformance moruna.net: ok:\n  exits 0\nlink node-supervisor.network-proof netstack-test.server\n")
        command([args.image_builder, description, args.user_bins, out / "assets"])
        home_path = args.io_home if args.io_phase else out / "home.img"
        if not args.io_phase:
            with (out / "home.img").open("xb") as home:
                home.truncate(64 * MIB)
            command([args.provision, "plan", "--output", out / "home.root", "--capacity", 64 * MIB,
                     "--alignment", 4096, "--log-bytes", MIB, "--role", "single-node-home", "--workspace", "moruna-ci", "--bucket", "jobs"])
            command([args.provision, "format", "--image", out / "home.img", "--root", out / "home.root"])
        if args.sdk_proof:
            # Descriptor paths are relative to ONE existing common owned root;
            # retain exact inputs without copying large layers per matrix case.
            # SDK inspect/snapshot is the sole descriptor/artifact validator.
            all_layers = [args.layer_fixture_1, args.layer_fixture_2, *layers]
            parent = Path(os.path.commonpath([str(path.parent) for path in
                                             [args.kernel_elf, out / "assets/initramfs.cpio", home_path, *all_layers]]))
            if parent == Path(parent.anchor) or parent.is_symlink() or parent.stat().st_uid != os.getuid():
                raise ValueError("SDK artifacts require a common owned non-root descriptor directory")
            def artifact(path):
                return {"path": path.relative_to(parent).as_posix(), "bytes": path.stat().st_size, "sha256": digest(path)}
            descriptor = {"version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
                          "kernel": artifact(args.kernel_elf), "initramfs": artifact(out / "assets/initramfs.cpio"),
                          "layers": [{"artifact": artifact(path)} for path in all_layers],
                          "writable_disks": [{"path": home_path.relative_to(parent).as_posix(), "bytes": home_path.stat().st_size, "serial": "node-ci-home"}],
                          "user_network": False, "cid": 3, "agent_port": PORT, "resources": resources}
            descriptor_path = parent / f".moruna-sdk-{run}.json"
            with descriptor_path.open("x") as file:
                json.dump(descriptor, file, indent=2); file.write("\n")
            # Evidence copy is byte-identical; actual dispatch uses the common-root descriptor.
            (out / "sdk-descriptor.json").write_bytes(descriptor_path.read_bytes())
            state = out / "state"; state.mkdir(mode=0o700)
            runtime = out / "runtime"; evidence = out / "sdk"
            if len(str(runtime / "vhost.sock").encode()) >= 100:
                raise ValueError("SDK owned runtime socket path too long")
            # Leave bounded time for the SDK's original cleanup and evidence writes.
            seconds = int(remaining()) - 30
            if seconds < 10:
                raise TimeoutError("insufficient remaining SDK execution/cleanup budget")
            config = {"descriptor": str(descriptor_path), "job": str(out / "job.json"), "image_digest": str(args.linux_digest),
                      "case": args.case, "node_boot": uuid.uuid4().hex, "attempt": 1,
                      "state_directory": str(state), "runtime_directory": str(runtime), "evidence_directory": str(evidence),
                      "qemu": str(args.qemu), "vsock": str(args.backend), "memory_bytes": 384 * MIB, "cpus": args.cpus,
                      "seconds": seconds, "inputs": {name: str(path) for name, path in input_files.items()}}
            config_path = out / "sdk-config.json"; config_path.write_text(json.dumps(config, indent=2) + "\n")
            record["case_inputs"] = {name: digest(out / name) for name in
                                     ("profile.json", "storage.conf", "network.conf", "canonical/engine.bindings", "canonical/storage.admission",
                                      "canonical/storage.grants", "canonical/identity.json", "assets/node.conf", "assets/engine.conf", "assets/initial.job",
                                      "assets/node-resources.json", "job.json", "assets/initramfs.cpio", "home.root", "sdk-descriptor.json", "sdk-config.json")}
            record.update({"engine_spec_digest": engine_spec_digest, "run_id": run, "kernel_sha256": digest(args.kernel_elf),
                           "job_sha256": digest(out / "job.json"), "initramfs_sha256": digest(out / "assets/initramfs.cpio"),
                           "mode": "explicit-test-tcg", "acceptance": "canonical-engine-task", "transport": "native-sdk"})
            with (out / "peer.log").open("xb") as log:
                subprocess.run([str(args.sdk_proof), str(config_path)], check=True, timeout=remaining(), stdout=log, stderr=subprocess.STDOUT)
            actual = json.loads((evidence / "result.json").read_text())
            terminal = json.loads((evidence / "terminal.json").read_text())
            key = json.loads((evidence / "prepared-key.json").read_text())
            phase = terminal.get("record", {}).get("phase")
            witness = state / f"{run}.launch"
            if (actual.get("passed") is not True or actual.get("case") != args.case or actual.get("cpus") != args.cpus
                    or actual.get("mode") != "explicit-test-tcg" or actual.get("key") != key or key.get("run_id") != run
                    or key.get("spec_digest") != engine_spec_digest or actual.get("terminal") != terminal.get("record")
                    or terminal.get("record", {}).get("key") != key or not isinstance(phase, dict) or set(phase) != {"Exited"}
                    or type(phase["Exited"]) is not int or not terminal.get("probe", "").startswith("Exited(")
                    or not witness.is_file() or witness.is_symlink() or witness.stat().st_size == 0):
                raise RuntimeError("actual SDK result/key/terminal/durable witness missing or inconsistent")
            sdk_cleanup = True
            record["sdk"] = {"descriptor_path": str(descriptor_path), "descriptor_sha256": digest(descriptor_path),
                             "key": key, "witness_path": str(witness), "witness_sha256": digest(witness),
                             "result_sha256": digest(evidence / "result.json"), "terminal_sha256": digest(evidence / "terminal.json")}
            # Preserve the SDK owner's real logs under original checker names.
            (out / "serial.log").write_bytes((runtime / "serial.log").read_bytes())
            serial = (out / "serial.log").read_text(errors="replace")
            if f"kk: {args.cpus} CPUs online" not in serial or "panic" in serial.lower() or "FAIL:" in serial:
                raise RuntimeError("actual SDK CPU-online proof missing or native panic observed")
            record["cleanup_method"] = "sdk-owned-exited"
            record["passed"] = True
            return
        elf = bytearray(args.kernel_elf.read_bytes())
        if elf[:6] != b"\x7fELF\x02\x01" or elf[18:20] != b"\x3e\x00":
            raise ValueError("actual original x86_64 kernel ELF required")
        elf[18:20] = b"\x03\x00"  # Existing QEMU direct Multiboot loader test convention.
        (out / "kernel.qemu.elf").write_bytes(elf)
        record["case_inputs"] = {name: digest(out / name) for name in
                                 ("profile.json", "storage.conf", "network.conf",
                                  "canonical/engine.bindings", "canonical/storage.admission", "canonical/storage.grants",
                                  "canonical/identity.json", "assets/node.conf", "assets/engine.conf", "assets/initial.job",
                                  "assets/node-resources.json", "job.json", "assets/initramfs.cpio", "kernel.qemu.elf")}
        record.update({"engine_spec_digest": engine_spec_digest, "run_id": run, "kernel_sha256": digest(args.kernel_elf), "job_sha256": digest(out / "job.json"),
                       "initramfs_sha256": digest(out / "assets/initramfs.cpio"), "mode": "explicit-test-tcg", "acceptance": "native-storage-network" if args.io_phase else "canonical-engine-task"})
        if args.io_phase:
            record["case_inputs"]["io.image"] = digest(description)
        else:
            record["case_inputs"]["home.root"] = digest(out / "home.root")
        backend = spawn([args.backend, "--socket", out / "vhost.sock", "--cid", 3, "--uds", out / "guest.uds"], "backend.log")
        while not (out / "vhost.sock").exists():
            remaining()
            if backend.poll() is not None:
                raise RuntimeError("actual vsock backend exited before binding")
            time.sleep(0.01)
        qemu_args = [args.qemu, "-accel", "tcg", "-cpu", "max", "-machine", "q35", "-m", f"{RAM // MIB}M",
                     "-smp", args.cpus, "-display", "none", "-monitor", "none", "-no-reboot", "-nodefaults",
                     "-serial", f"file:{out / 'serial.log'}", "-qmp", f"unix:{out / 'qmp.sock'},server=on,wait=off",
                     "-object", f"memory-backend-memfd,id=ram0,size={RAM},share=on", "-numa", "node,memdev=ram0",
                     "-chardev", f"socket,id=vsock0,path={out / 'vhost.sock'}", "-device", "vhost-user-vsock-pci,chardev=vsock0,addr=0x6",
                     "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04", "-kernel", out / "kernel.qemu.elf", "-initrd", out / "assets/initramfs.cpio",
                     "-drive", f"file={home_path},format=raw,if=none,id=home0", "-device", "nvme,drive=home0,serial=node-ci-home,addr=0x5"]
        for number, path in enumerate([args.layer_fixture_1, args.layer_fixture_2, *layers], 1):
            address = 7 + number
            qemu_args += ["-drive", f"file={path},format=raw,if=none,id=layer{number},readonly=on", "-device", f"virtio-blk-pci,drive=layer{number},addr=0x{address:x},disable-legacy=on"]
        if args.io_phase:
            ready = json.loads((args.io_net / "peer-ready.json").read_text())
            port = ready["port"]
            if type(port) is not int or not 1 <= port <= 65535 or ready["host"] != "127.0.0.1":
                raise ValueError("actual controlled KE2 peer endpoint required")
            qemu_args += ["-netdev", f"user,id=ke2-user,restrict=on,dhcpstart=10.0.2.15,guestfwd=tcp:10.0.2.100:47012-tcp:127.0.0.1:{port}", "-device", "virtio-net-pci,netdev=ke2-user,mac=52:54:00:4b:45:02,romfile=,addr=0x14,disable-legacy=on"]
        qemu = spawn(qemu_args, "qemu.log")
        if args.io_phase:
            required = ["KE2 user DHCP: ok:", "KE2 user DNS: ok:", "KE2 user TLS: ok:", "KE2 conformance moruna.net: ok:",
                        f"KE3 native storage: {args.io_phase}:", "KE3 native storage: ok:",
                        "kk: process engine exited with code 0", "kk: process netstack-test exited with code 0"]
            while True:
                remaining()
                serial = (out / "serial.log").read_text(errors="replace") if (out / "serial.log").exists() else ""
                if "FAIL:" in serial or "panic" in serial.lower() or qemu.poll() is not None:
                    raise RuntimeError("native IO fixture failed before all required facts")
                if all(marker in serial for marker in required):
                    break
                time.sleep(0.05)
        else:
            command([args.peer, out / "peer.log", out / "guest.uds", PORT, args.linux_digest, "--case", args.case, "--job", out / "job.json"])
        serial = (out / "serial.log").read_text(errors="replace")
        if f"kk: {args.cpus} CPUs online" not in serial or "panic" in serial.lower() or "FAIL:" in serial:
            raise RuntimeError("actual CPU-online proof missing or native panic observed")
        # Actual natural33 and actual QMP-requested0 are distinct, recorded facts.
        # Neither is fabricated for a resident node or a failed peer job.
        status = qemu.poll()
        qmp_acknowledged = False
        if status is None:
            try:
                with socket.socket(socket.AF_UNIX) as qmp:
                    qmp.settimeout(min(5, remaining()))
                    qmp.connect(str(out / "qmp.sock"))
                    stream = qmp.makefile("rwb", buffering=0)
                    if "QMP" not in json.loads(stream.readline(65536)):
                        raise RuntimeError("actual QMP greeting missing")
                    for method, identity in (("qmp_capabilities", 1), ("quit", 2)):
                        stream.write(json.dumps({"execute": method, "id": identity}).encode() + b"\n")
                        while True:
                            reply = json.loads(stream.readline(65536))
                            if reply.get("id") == identity:
                                if "error" in reply:
                                    raise RuntimeError(f"QMP refused {method}: {reply['error']}")
                                break
                    qmp_acknowledged = True
            except (OSError, ValueError):
                # Guest completion may race the QMP socket. Only the actual
                # debug-exit success status below can establish this alternative.
                if qemu.poll() != 33:
                    raise
            status = qemu.wait(timeout=min(10, remaining()))
        record["qemu_exit"] = status
        record["cleanup_method"] = "guest-normal33" if status == 33 else "qmp-quit" if qmp_acknowledged else "unknown"
        if status != 33 and not (status == 0 and qmp_acknowledged):
            raise RuntimeError("no actual natural-success or acknowledged owned QMP cleanup")
        record["passed"] = True
    except Exception as error:
        record["error"] = str(error)
    finally:
        cleaned = sdk_cleanup
        for child in reversed(children):
            try:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=10)
            except (OSError, subprocess.TimeoutExpired):
                cleaned = False
        for log in logs:
            log.close()
        record["owned_children"] = [{"pid": child.pid, "returncode": child.returncode} for child in children]
        try:
            after = {name: digest(path) for name, path in input_files.items()}
            inputs_unchanged = bool(input_files) and after == record.get("inputs") and not (args.engine_layer_dir / f"stack-{len(layers) + 1}.img").exists()
            case_unchanged = bool(record.get("case_inputs")) and all(digest(out / name) == expected for name, expected in record["case_inputs"].items())
            record["inputs_unchanged"] = inputs_unchanged
            record["case_inputs_unchanged"] = case_unchanged
            if not inputs_unchanged or not case_unchanged:
                record["passed"] = False
                record["input_integrity_error"] = "immutable input or retained per-case artifact changed"
        except (OSError, ValueError) as error:
            record["passed"] = False
            record["input_integrity_error"] = str(error)
        if args.sdk_proof and record.get("sdk"):
            try:
                sdk = record["sdk"]
                if digest(Path(sdk["descriptor_path"])) != sdk["descriptor_sha256"] or digest(Path(sdk["witness_path"])) != sdk["witness_sha256"]:
                    raise ValueError("SDK descriptor or terminal witness changed")
            except (OSError, ValueError) as error:
                record["passed"] = False
                record["input_integrity_error"] = str(error)
        record["cleanup"] = cleaned
        record["passed"] = bool(record["passed"] and cleaned)
        (out / "result.json").write_text(json.dumps(record, indent=2) + "\n")
        if args.sdk_proof and not record["passed"]:
            raise SystemExit(f"SDK node CI case failed; actual evidence: {out / 'result.json'}")
    if not record["passed"]:
        raise SystemExit(f"node CI case failed; actual evidence: {out / 'result.json'}")


if __name__ == "__main__":
    main()
