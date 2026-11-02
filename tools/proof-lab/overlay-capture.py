"""Three-way overlay coherent source-capture deltas into a private candidate."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

candidate = Path(sys.argv[1]).resolve(strict=True)
capture = Path(sys.argv[2]).resolve(strict=True)
previous = Path(sys.argv[3]).resolve(strict=True) if sys.argv[3] != '-' else None
prefixes = sys.argv[4:]
source = candidate / 'source'
repo = Path('/Users/brackly/Desktop/Projects/moruna-v1')

def sha(data):
    return hashlib.sha256(data).hexdigest()

def manifest(folder):
    origin = json.loads((folder / 'origin.json').read_text())
    raw = (folder / 'source-manifest.json').read_bytes()
    assert sha(raw) == origin['source_census_sha256']
    entries = json.loads(raw)
    for entry in entries:
        assert sha((folder / 'source' / entry['path']).read_bytes()) == entry['sha256']
    return origin, {e['path']: e for e in entries}

origin, current = manifest(capture)
if previous:
    prior_origin, before = manifest(previous)
else:
    prior_origin = None
    raw = subprocess.check_output(['git','ls-tree','-r','-z',origin['source_head']], cwd=repo)
    before = {}
    for line in raw.split(b'\0'):
        if line:
            meta, name = line.split(b'\t',1)
            mode, kind, oid = meta.decode().split()
            assert kind == 'blob'
            before[os.fsdecode(name)] = {'oid':oid, 'mode':int(mode,8) & 0o777}

def old_bytes(name):
    return ((previous/'source'/name).read_bytes() if previous else
            subprocess.check_output(['git','cat-file','blob',before[name]['oid']], cwd=repo))

applied, conflicts = [], []
for name in sorted(set(before)|set(current)):
    if prefixes and not any(name.startswith(prefix) for prefix in prefixes):
        continue
    relative = Path(name)
    if any(part in {'.git','.vmrun','.venv','node_modules'} for part in relative.parts):
        continue
    if relative.parts[0] in {'target','target-linux'} or relative.parts[0].startswith('target-'):
        continue
    if len(relative.parts)>1 and relative.parts[0]=='kernel' and (relative.parts[1]=='target' or relative.parts[1].startswith('target-')):
        continue
    old = old_bytes(name) if name in before else None
    new = (capture/'source'/name).read_bytes() if name in current else None
    if old == new and (new is None or before[name]['mode'] == current[name]['mode']):
        continue
    dest = source/name
    existing = dest.read_bytes() if dest.exists() else None
    if existing == new:
        continue
    if new is None:
        if existing == old:
            dest.unlink()
            applied.append(name)
        else:
            conflicts.append({'path':name, 'kind':'modify/delete'})
        continue
    dest.parent.mkdir(parents=True,exist_ok=True)
    if existing is None or existing == old:
        dest.write_bytes(new)
    elif old is None:
        conflicts.append({'path':name,'kind':'add/add'})
        continue
    else:
        with tempfile.TemporaryDirectory(prefix='merge-',dir=candidate) as temp:
            base, incoming = Path(temp)/'base', Path(temp)/'incoming'
            base.write_bytes(old)
            incoming.write_bytes(new)
            result=subprocess.run(['git','merge-file','-p','-L','integration','-L','previous-input',
                                   '-L',capture.name,str(dest),str(base),str(incoming)],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            assert 0 <= result.returncode < 128, result.stderr.decode()
            dest.write_bytes(result.stdout)
            if result.returncode:
                conflicts.append({'path':name,'kind':'content','hunks':result.returncode})
    dest.chmod(current[name]['mode'])
    applied.append(name)
record={'capture':origin,'previous':prior_origin,'applied':applied,'conflicts':conflicts}
out=candidate/('capture-overlay-'+capture.name+'.json')
assert not out.exists()
out.write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({'candidate':str(candidate),'capture':str(capture),'applied':len(applied),'conflicts':conflicts}))
