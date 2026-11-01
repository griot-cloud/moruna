"""Capture a coherent local feature source tree for an integration proof, never builds."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

os.umask(0o077)
root = Path(__file__).resolve().parent
repo = Path(sys.argv[1]).resolve(strict=True)
assert repo.parent == Path('/Users/brackly/Desktop/Projects') and repo.name.startswith('moruna-')
label = sys.argv[2]
assert label.replace('-', '').isalnum()
out = Path(tempfile.mkdtemp(prefix='source-capture-' + label + '-', dir=root))
source = out / 'source'
source.mkdir(mode=0o700)

def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def selected():
    files = []
    raw = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=repo)
    for item in raw.split(b'\0'):
        if not item:
            continue
        rel = Path(os.fsdecode(item))
        # Only known build-output roots are excluded. Source files such as
        # target_feature_fallbacks.rs inside pinned third-party code are real input.
        if any(part in {'.git', '.vmrun', '.venv', 'node_modules'} for part in rel.parts):
            continue
        if rel.parts[0] in {'target', 'target-linux'} or rel.parts[0].startswith('target-'):
            continue
        if len(rel.parts)>1 and rel.parts[0]=='kernel' and (rel.parts[1]=='target' or rel.parts[1].startswith('target-')):
            continue
        path = repo / rel
        if not path.exists():
            continue
        info = path.lstat()
        assert stat.S_ISREG(info.st_mode) and not path.is_symlink(), str(rel)
        files.append({'path': str(rel), 'type': 'file', 'bytes': info.st_size,
                      'mode': stat.S_IMODE(info.st_mode), 'sha256': sha(path)})
    return sorted(files, key=lambda item: item['path'])

before = selected()
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()
diff = subprocess.check_output(['git', 'diff', '--binary', 'HEAD'], cwd=repo)
for item in before:
    dest = source / item['path']
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(repo / item['path'], dest)
    assert sha(dest) == item['sha256']
assert selected() == before, 'source changed during capture'
assert subprocess.check_output(['git', 'diff', '--binary', 'HEAD'], cwd=repo) == diff
manifest = out / 'source-manifest.json'
manifest.write_text(json.dumps(before, sort_keys=True, indent=2) + '\n')
record = {'repository': str(repo), 'source_head': head,
          'source_dirty_diff_sha256': hashlib.sha256(diff).hexdigest(),
          'source_census_sha256': sha(manifest), 'source_files': len(before),
          'source_unchanged_during_copy': True, 'capture_path': str(out)}
(out / 'origin.json').write_text(json.dumps(record, indent=2) + '\n')
(out / 'dirty.patch').write_bytes(diff)
print(json.dumps(record))
