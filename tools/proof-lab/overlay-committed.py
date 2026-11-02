"""Three-way feature history overlay for a private proof candidate, no branch/commit mutation."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

repo = Path('/Users/brackly/Desktop/Projects/moruna-v1')
candidate = Path(sys.argv[1]).resolve(strict=True)
source = candidate / 'source'
initial = '38447a99447151aec77eb28272a8a32e682052f3'
features = sys.argv[2:]
parents = [initial, 'f8a262a829bc7d4f53e03b3d88f4cd644ea88133',
           '45497efeae517434207e30c9e31b778f3404419d']
prior = sorted(candidate.glob('foundation-overlays*.json'))
for ledger in prior:
    parents.extend(item['head'] for item in json.loads(ledger.read_text())['features'])
path = candidate / ('foundation-overlays.json' if not prior else
                    'foundation-overlays-%02d.json' % (len(prior) + 1))
assert not path.exists(), 'never overwrite prior candidate integration evidence'

def git(*args):
    return subprocess.check_output(['git', *args], cwd=repo)

def tree(ref):
    result = {}
    for line in git('ls-tree', '-r', '-z', ref).split(b'\0'):
        if not line:
            continue
        meta, path = line.split(b'\t', 1)
        mode, kind, oid = meta.decode().split()
        assert kind == 'blob'
        result[path.decode()] = (mode, oid)
    return result

record = {'state': 'source candidate only; not a proof or branch merge', 'features': []}
for feature in features:
    bases = git('merge-base', '--all', feature, *parents).decode().splitlines()
    assert len(bases) == 1, 'criss-cross ancestry requires an explicit virtual base'
    base = bases[0]
    before, after = tree(base), tree(feature)
    changes, conflicts = [], []
    for name in sorted(set(before) | set(after)):
        old, new = before.get(name), after.get(name)
        if old == new:
            continue
        dest = source / name
        if new is None:
            if dest.exists() and dest.read_bytes() == git('cat-file', 'blob', old[1]):
                dest.unlink()
            elif dest.exists():
                conflicts.append({'path': name, 'reason': 'modified versus deletion'})
            continue
        assert new[0] in ('100644', '100755'), name
        incoming = git('cat-file', 'blob', new[1])
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not dest.exists():
            dest.write_bytes(incoming)
        elif dest.read_bytes() == incoming:
            continue
        elif old is None:
            conflicts.append({'path': name, 'reason': 'different additions'})
            continue
        elif dest.read_bytes() == git('cat-file', 'blob', old[1]):
            dest.write_bytes(incoming)
        else:
            original = candidate / 'overlay-base'
            other = candidate / 'overlay-incoming'
            original.write_bytes(git('cat-file', 'blob', old[1]))
            other.write_bytes(incoming)
            merged = subprocess.run(['git', 'merge-file', '-p', '-L', 'integration', '-L', base,
                                     '-L', feature, str(dest), str(original), str(other)],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if not 0 <= merged.returncode < 128:
                raise RuntimeError(merged.stderr.decode())
            dest.write_bytes(merged.stdout)
            if merged.returncode:
                conflicts.append({'path': name, 'reason': 'content', 'hunks': merged.returncode})
        dest.chmod(0o755 if new[0] == '100755' else 0o644)
        changes.append(name)
    record['features'].append({'head': feature, 'merge_base': base, 'applied': changes, 'conflicts': conflicts})
    parents.append(feature)
path.write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps({'candidate': str(candidate), 'features': [{'head': x['head'], 'applied': len(x['applied']),
                  'conflicts': x['conflicts']} for x in record['features']]}))
