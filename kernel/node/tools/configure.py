#!/usr/bin/env python3
"""Render explicit standalone assets; never fetch layers or create trust/credentials.

The profile is operator input. Canonical Rust parsers validate the generated files
again during package.py. No boot or hardware success is asserted by rendering.
"""
import argparse
import json
from pathlib import Path
import shutil


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    for name in ("image-record", "layers", "network", "tls-roots", "engine-bindings", "storage-config", "storage-grants", "storage-admission"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--kd3-conformance-proof", type=Path,
                        help="explicit TEST-ONLY JSON {program,args,nonce}; Rust feature-gated validator remains mandatory")
    parser.add_argument("--storage-credential", type=Path)
    for name in ("local-config","checkpoint-admission","cache-admission","cache-key"):
        parser.add_argument(f"--{name}", type=Path)
    parser.add_argument("--maintenance-target", choices=("storage", "local"))
    parser.add_argument("--maintenance-config", type=Path)
    args = parser.parse_args()
    if (args.maintenance_target is None) != (args.maintenance_config is None):
        parser.error("maintenance target and finite canonical config must be supplied together")
    try:
        profile = json.loads(args.profile.read_text())
    except (OSError, ValueError) as error:
        parser.error(f"cannot read profile: {error}")
    expected = {"version", "node_memory_bytes", "cpus", "agent_port", "job_memory_bytes", "job_account_bytes", "scratch_bytes", "engine_program", "service_accounts", "storage_provider"}
    if not isinstance(profile, dict) or set(profile) != expected or type(profile["version"]) is not int or profile["version"] != 1:
        parser.error("unknown or missing profile fields/version")
    unified = profile["storage_provider"] == "local-single-node-home"
    local_inputs=(args.local_config,args.checkpoint_admission,args.cache_admission,args.cache_key)
    if unified:
        if args.local_config is not None or any(value is None for value in local_inputs[1:]):
            parser.error("unified raw home requires both role Admissions/key and uses storage-config, not a second local-config")
    elif len([value for value in local_inputs if value is not None]) not in (0,4):
        parser.error("local config, both role Admissions and key must be supplied together")
    services = ["devices", "imagefs", "linux", "network", "admission", "storage"]
    if args.local_config is not None: services.append("local")
    if args.maintenance_target is not None:
        if args.maintenance_target not in services:
            parser.error("maintenance target must be an actual selected storage instance")
        services.append("housekeeping")
    if not isinstance(profile["service_accounts"], dict) or set(profile["service_accounts"]) != set(services):
        parser.error("exact accounts for every selected actual service are required")
    numbers = [profile[key] for key in expected - {"version", "engine_program", "service_accounts", "storage_provider"}]
    numbers += list(profile["service_accounts"].values())
    if any(type(value) is not int or value < 0 for value in numbers):
        parser.error("budgets, CPUs and port must be nonnegative integers")
    page_values = [profile[k] for k in ("node_memory_bytes", "job_memory_bytes", "job_account_bytes", "scratch_bytes")]
    page_values += list(profile["service_accounts"].values())
    if any(value % 4096 for value in page_values) or any(profile["service_accounts"][s] == 0 for s in services):
        parser.error("service and resource accounts must be whole pages")
    if not 1 <= profile["cpus"] <= 64 or not 1 <= profile["agent_port"] <= 0xffffffff:
        parser.error("invalid CPU count or runtime vsock port")
    if profile["node_memory_bytes"] < 256 * 1024 * 1024 or profile["node_memory_bytes"] % (2 * 1024 * 1024) or profile["job_memory_bytes"] == 0 or profile["scratch_bytes"] == 0:
        parser.error("invalid fixed node/job memory contract")
    if profile["job_memory_bytes"] + profile["scratch_bytes"] >= profile["job_account_bytes"]:
        parser.error("job account must include runtime, scratch and native engine overhead")
    if profile["service_accounts"]["admission"] <= profile["job_account_bytes"]:
        parser.error("admission account must include child and own overhead")
    if sum(profile["service_accounts"].values()) >= profile["node_memory_bytes"]:
        parser.error("node RAM must include all carved service accounts and boot overhead")
    provider = profile["storage_provider"]
    if provider not in ("bucket", "single-node-home", "local-single-node-home"):
        parser.error("storage_provider must explicitly select bucket, single-node-home or local-single-node-home")
    if args.maintenance_target == "storage" and provider == "bucket":
        parser.error("maintenance must target actual local raw owner, never bucket proxy")
    if (provider == "bucket") != (args.storage_credential is not None):
        parser.error("only bucket requires an explicit issued storage credential input")
    program = profile["engine_program"]
    if not isinstance(program, str) or not program.startswith("/") or program.endswith("/") or any(c.isspace() or c in "#\x00" for c in program):
        parser.error("engine_program must be one exact absolute guest path")
    if args.out.exists():
        parser.error("output must be a new directory")
    sources = [(args.image_record, "image.record"), (args.layers, "linux.layers"), (args.network, "network.conf"), (args.tls_roots, "tls.roots"), (args.engine_bindings, "engine.bindings"), (args.storage_config, "storage.conf"), (args.storage_grants, "storage.grants"), (args.storage_admission, "storage.admission")]
    if args.maintenance_config is not None:
        sources.append((args.maintenance_config, "maintenance.conf"))
    if args.storage_credential is not None:
        sources.append((args.storage_credential, "storage.credential"))
    if args.local_config is not None: sources.append((args.local_config,"local.conf"))
    if args.checkpoint_admission is not None:
        sources += [(args.checkpoint_admission,"checkpoint.admission"),(args.cache_admission,"cache.admission"),(args.cache_key,"cache.key")]
    for source, _ in sources:
        if source.is_symlink() or not source.is_file():
            parser.error(f"explicit regular input unavailable: {source}")
    args.out.mkdir(mode=0o700)
    for source, name in sources:
        shutil.copyfile(source, args.out / name)
    node = ["moruna-node 1"]
    images = {"devices": "image.devices", "imagefs": "image.imagefs", "linux": "image.linux", "network": "image.netstack", "admission": "image.admission", "storage": "image.storage", "local":"image.storage", "housekeeping":"image.maintenance"}
    node += [f"service {s} {images[s]} {profile['service_accounts'][s]}" for s in services]
    node += ["bootstrap devices devices", "bootstrap linux linux", "bootstrap admission admission", "grant devices drivers drivers"]
    node += [f"grant devices driver.{driver} driver.{driver}" for driver in ("virtio-blk", "virtio-vsock", "virtio-net", "nvme", "gve")]
    node += [
        "grant imagefs layers linux.layers", "grant network config network.config", "grant network tls-roots tls.roots",
        "grant admission root.process root.process", "grant admission engine-image image.engine",
        "grant admission initial-job initial.job", "grant admission initial-config engine.config", "grant admission initial.image initial.image",
        "grant admission initial.bindings engine.bindings",
        "grant admission storage.grants storage.grants",
        "grant admission initial.storage-binding storage.admission",
        "grant storage storage.config storage.config",
        "link storage storage.admission admission initial.storage-admission",
        "link devices directory-fs imagefs directory", "link linux fs imagefs listen-linux",
        "link devices directory-net network directory", "link devices directory-engine admission initial.directory",
        "link linux listen-engine admission initial.linux", "link network listen-engine admission initial.network",
    ]
    node += [f"link storage storage.authority admission {'local-home.authority' if unified else 'storage.authority'}"]
    if provider == "bucket":
        node += ["grant storage storage.credential storage.credential", "link network listen-storage storage storage.network-listener"]
    else:
        node += ["link devices directory-storage storage storage.block-directory"]
    extra_initial = []
    extra_engine = []
    if unified:
        node += ["grant storage storage.key cache.key", "grant admission initial.checkpoint-binding checkpoint.admission", "grant admission initial.cache-binding cache.admission"]
        extra_initial += ["grant checkpoint.binding initial.checkpoint-binding", "grant cache.binding initial.cache-binding"]
        extra_engine += ["grant checkpoint admission storage.admission checkpoint.binding", "grant cache admission storage.admission cache.binding"]
    if "local" in services:
        node += ["grant local storage.config local.config", "grant local storage.key cache.key",
                 "grant admission initial.checkpoint-binding checkpoint.admission", "grant admission initial.cache-binding cache.admission",
                 "link local storage.authority admission local.authority", "link local storage.admission admission initial.local-admission",
                 "link devices directory-local local storage.block-directory"]
        extra_initial += ["grant local.admission initial.local-admission", "grant checkpoint.binding initial.checkpoint-binding", "grant cache.binding initial.cache-binding"]
        extra_engine += ["grant checkpoint admission local.admission checkpoint.binding", "grant cache admission local.admission cache.binding"]
    if args.maintenance_target is not None:
        node += [f"link {args.maintenance_target} storage.maintenance housekeeping storage.maintenance",
                 f"grant {args.maintenance_target} storage.maintenance-config maintenance.server-config",
                 "grant housekeeping storage.maintenance-config maintenance.client-config"]
    (args.out / "node.conf").write_text("\n".join(node) + "\n")
    (args.out / "initial.job").write_text(f"moruna-node-job 1\nbudget {profile['job_account_bytes']}\ngrant linux initial.linux\ngrant directory initial.directory\ngrant image initial.image\ngrant engine.bindings initial.bindings\nservice network initial.network\ngrant storage.admission initial.storage-admission\ngrant storage.binding initial.storage-binding\n")
    (args.out / "engine.conf").write_text(
        f"program {program}\narg serve\narg --listen\narg vsock://-1:{profile['agent_port']}\n"
        f"env HOME=/tmp\nmemory {profile['job_memory_bytes']}\nscratch {profile['scratch_bytes']}\n"
        "grant vsock lookup vsock0\ngrant linux personality\ngrant image data image\ngrant engine.bindings data engine.bindings\ngrant network service network\ngrant storage admission storage.admission storage.binding\nruns 1\n")
    if args.kd3_conformance_proof:
        command = json.loads(args.kd3_conformance_proof.read_text())
        if set(command) != {"program", "args", "nonce"} or not isinstance(command["args"], list):
            parser.error("explicit proof command requires exact program,args,nonce")
        values = [command["program"], command["nonce"], *command["args"]]
        if any(not isinstance(v, str) or not v or any(c.isspace() or c in "#\x00" for c in v) for v in values):
            parser.error("unsafe proof command token")
        config = args.out / "engine.conf"
        lines = config.read_text().splitlines()
        lines = [line for line in lines if not line.startswith(("program ", "arg "))]
        config.write_text("\n".join(["program " + command["program"],
            *("arg " + value for value in command["args"]),
            "env MORUNA_KD3_PROOF_NONCE=" + command["nonce"], *lines]) + "\n")
    with (args.out / "initial.job").open("a") as output: output.write("\n".join(extra_initial) + ("\n" if extra_initial else ""))
    with (args.out / "engine.conf").open("a") as output: output.write("\n".join(extra_engine) + ("\n" if extra_engine else ""))
    (args.out / "profile.json").write_text(json.dumps(profile, indent=2) + "\n")


if __name__ == "__main__":
    main()
