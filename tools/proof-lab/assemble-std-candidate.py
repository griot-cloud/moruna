"""Prepare a three-way integration source candidate; never commits or builds."""
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile

os.umask(0o077)
root = Path(__file__).resolve().parent
repo = Path('/Users/brackly/Desktop/Projects/moruna-v1')
out = Path(tempfile.mkdtemp(prefix='std-integration-candidate-', dir=root))
source = out / 'source'
source.mkdir(mode=0o700)

def git(*args):
    return subprocess.check_output(['git', *args], cwd=repo)

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

left = 'f8a262a829bc7d4f53e03b3d88f4cd644ea88133'
right = '45497efeae517434207e30c9e31b778f3404419d'
merge = subprocess.run(['git', 'merge-tree', '--write-tree', '--name-only', left, right],
                       cwd=repo, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
assert merge.returncode in (0, 1), merge.stdout
tree = merge.stdout.splitlines()[0]
assert re.fullmatch('[0-9a-f]{40}', tree)
(out / 'committed-merge.log').write_text(merge.stdout)
with tarfile.open(fileobj=io.BytesIO(git('archive', '--format=tar', tree))) as archive:
    for member in archive:
        path = Path(member.name)
        assert not path.is_absolute() and '..' not in path.parts
        assert member.isfile() or member.isdir(), member.name
    archive.extractall(source, filter='data')

conflicts = []
origins = []
applied = []

def selected(path):
    return not any(part in {'.git', '.vmrun', '.venv', 'node_modules'} or part.startswith('target') for part in Path(path).parts)

def overlay(capture, accept):
    origin = json.loads((capture / 'origin.json').read_text())
    assert sha(capture / 'source-manifest.json') == origin['source_census_sha256']
    manifest = json.loads((capture / 'source-manifest.json').read_text())
    current = {item['path']: item for item in manifest}
    base = {}
    for raw in git('ls-tree', '-r', '-z', origin['source_head']).split(b'\0'):
        if not raw:
            continue
        meta, name = raw.split(b'\t', 1)
        mode, kind, oid = meta.decode().split()
        assert kind == 'blob'
        base[os.fsdecode(name)] = (mode, oid)
    for name in sorted(set(base) | set(current)):
        if not accept(name) or not selected(name):
            continue
        previous = base.get(name)
        item = current.get(name)
        dest = source / name
        if item is None:
            if not dest.exists():
                continue
            original = git('cat-file', 'blob', previous[1])
            if dest.read_bytes() == original:
                dest.unlink()
                applied.append({'capture': str(capture), 'path': name, 'operation': 'delete'})
            else:
                conflicts.append({'capture': str(capture), 'path': name, 'reason': 'modified versus delete'})
            continue
        path = capture / 'source' / name
        assert sha(path) == item['sha256']
        data = path.read_bytes()
        blob = hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
        mode = '100755' if item['mode'] & 0o111 else '100644'
        if previous == (mode, blob):
            continue
        dest.parent.mkdir(parents=True, exist_ok=True)
        if previous is None or not dest.exists():
            if dest.exists() and dest.read_bytes() != data:
                conflicts.append({'capture': str(capture), 'path': name, 'reason': 'different additions'})
                continue
            dest.write_bytes(data)
        else:
            original = git('cat-file', 'blob', previous[1])
            if dest.read_bytes() == original or dest.read_bytes() == data:
                dest.write_bytes(data)
            else:
                base_file = out / 'base-file'
                base_file.write_bytes(original)
                merged = subprocess.run(['git', 'merge-file', '-p', '-L', 'integration', '-L', 'feature-base',
                                         '-L', capture.name, str(dest), str(base_file), str(path)],
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                assert merged.returncode in (0, 1), merged.stderr.decode()
                dest.write_bytes(merged.stdout)
                if merged.returncode:
                    conflicts.append({'capture': str(capture), 'path': name, 'reason': 'three-way content conflict'})
        dest.chmod(item['mode'])
        applied.append({'capture': str(capture), 'path': name, 'operation': 'merge/update'})
    origins.append(origin)

overlay(root / 'source-capture-runtimeportable-kvvfqg2w', lambda name: not name.startswith('kernel/'))
overlay(root / 'source-capture-storagecurrent-5m4ff6q8', lambda name: not name.startswith('kernel/'))
host_files = {'crates/moruna-runtime/src/host/' + name for name in ('native.rs', 'protocol.rs', 'cli.rs', 'mod.rs')}
host_files |= {'crates/moruna-vmm/src/' + name for name in ('microkernel.rs', 'lib.rs')}
overlay(root / 'source-capture-nodehost-ubeiimvy', lambda name: name in host_files)
record = {'state': 'draft source candidate, not a branch merge or proof pass', 'committed_merge_tree': tree,
          'committed_parents': [left, right], 'source': str(source), 'origins': origins,
          'applied': applied, 'conflicts': conflicts,
          'known_committed_conflict': 'architecture/sdd/24-cost.md invariants/tests require renumbering at integration'}
(out / 'integration-provenance.json').write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps({'path': str(out), 'applied_files': len(applied), 'conflicts': conflicts}))
