"""Recorded integration conflict resolution in an owned source candidate only."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import sys

candidate = Path(sys.argv[1]).resolve(strict=True)
source = candidate / 'source'
root = Path(__file__).resolve().parent
record = []

def update(name, data, reason):
    path = source / name
    before = hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(data)
    record.append({'path': name, 'before': before, 'after': hashlib.sha256(path.read_bytes()).hexdigest(), 'reason': reason})

def conflicts(name, choose):
    data = (source / name).read_text()
    pattern = r'^<<<<<<<[^\n]*\n(.*?)^=======\n(.*?)^>>>>>>>[^\n]*\n'
    index = 0
    def replace(match):
        nonlocal index
        index += 1
        return choose(index, match[1], match[2])
    resolved = re.sub(pattern, replace, data, flags=re.M | re.S)
    assert not re.search(r'^<<<<<<<|^=======|^>>>>>>>', resolved, re.M), name
    update(name, resolved, 'three-way conflict resolution preserving both feature contracts')

conflicts('crates/moruna-scheduler/src/sink_drive.rs',
          lambda n,a,b: b.replace('poll_writes(shared, end, sink,', 'poll_writes(shared, end, queue, sink,'))
conflicts('crates/moruna-runtime/src/host/cli.rs',
          lambda n,a,b: a.replace('--image <dir-or-oci> [--disk <path>[:ro|:rw]]...', '--image <node-image.json>'))

def manifest_union(n,a,b):
    lines = a.splitlines(keepends=True)
    # Repeated workspace dependency keys are a single canonical declaration.
    keys = {line.split('=',1)[0].strip() for line in lines if '=' in line and not line.lstrip().startswith('#')}
    for line in b.splitlines(keepends=True):
        key = line.split('=',1)[0].strip() if '=' in line and not line.lstrip().startswith('#') else None
        if key is None or key not in keys:
            lines.append(line)
            if key:
                keys.add(key)
    return ''.join(lines)

conflicts('Cargo.toml', manifest_union)
conflicts('kernel/plane/core/Cargo.toml', manifest_union)
conflicts('kernel/plane/core/src/lib.rs', lambda n,a,b: a.rsplit('#[cfg(target_os',1)[0] + b)
conflicts('kernel/Cargo.toml', lambda n,a,b: a.replace('"engine"]', '"engine", "net/*"]'))
conflicts('kernel/svc/src/lib.rs', lambda n,a,b: a.replace('println, start,', 'println, random, start,'))
conflicts('crates/moruna-runtime/src/kinds/image.rs', lambda n,a,b: a if n == 1 else a+b)

# Use the complete pinned storage lock as the resolver seed. Cargo will add actual
# local prerequisite packages, and external entries must be audited unchanged.
lock = root / 'source-capture-storagecurrent-5m4ff6q8/source/Cargo.lock'
update('Cargo.lock', lock.read_text(), 'complete registry-pinned seed; local graph will be resolved offline and externally audited')

# This is generated from the declaration by the catalogue gate after compilation.
catalogue = root / 'source-capture-ka2integration-ai3ts3kx/source/crates/moruna-job/catalogue/standard.json'
update('crates/moruna-job/catalogue/standard.json', catalogue.read_text(), 'canonical committed generated file pending declaration-driven regeneration; no hand edit')

path = candidate / 'std-resolution.json'
assert not path.exists()
path.write_text(json.dumps({'state': 'partial source integration; not compiled, not a branch merge', 'resolutions': record}, indent=2) + '\n')
print(json.dumps({'candidate': str(candidate), 'resolved_files': len(record)}))
