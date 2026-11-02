import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tarfile
import tempfile

os.umask(0o077)
root = Path(__file__).resolve().parent
capture = Path(sys.argv[1]).resolve(strict=True)
assert capture.parent == root and capture.name.startswith('source-capture-ke2')
origin = json.loads((capture / 'origin.json').read_text())
old = root / 'ke2-source-nv1e357m'
out = Path(tempfile.mkdtemp(prefix='ke2-source-v2-', dir=root))
source = out / 'inputs'
source.mkdir(mode=0o700)

def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def save(name, value):
    path = out / name
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')
    return sha(path)

manifest = json.loads((capture / 'source-manifest.json').read_text())
assert sha(capture / 'source-manifest.json') == origin['source_census_sha256']
selected = [item for item in manifest if item['path'].startswith('kernel/')
            or item['path'] in {'LICENSE', 'NOTICE', 'architecture/sdd/31-network-server.md'}]
for item in selected:
    path = capture / 'source' / item['path']
    assert sha(path) == item['sha256']
    dest = source / item['path']
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, dest)
lock = sha(source / 'kernel/Cargo.lock')
assert lock == sha(old / 'inputs/kernel/Cargo.lock'), 'new external graph needs independently verified vendor'
shutil.copytree(old / 'inputs/vendor', source / 'vendor', copy_function=os.link)
(source / '.cargo').mkdir(mode=0o700)
shutil.copy2(old / 'inputs/.cargo/config.toml', source / '.cargo/config.toml')
source_sha = save('source-manifest.json', selected)
items = []
for path in sorted(source.rglob('*')):
    info = path.lstat()
    assert not path.is_symlink()
    if path.is_file():
        items.append({'path': str(path.relative_to(source)), 'type': 'file',
                      'bytes': info.st_size, 'mode': stat.S_IMODE(info.st_mode), 'sha256': sha(path)})
inputs_sha = save('inputs-manifest.json', items)
archive = out / 'ke2-inputs.tar.gz'
with tarfile.open(archive, 'w:gz', compresslevel=1) as tar:
    for item in items:
        path = source / item['path']
        member = tar.gettarinfo(str(path), arcname=item['path'])
        member.uid = member.gid = 0
        member.uname = member.gname = ''
        member.mtime = 0
        with path.open('rb') as file:
            tar.addfile(member, file)
record = {'source_head': origin['source_head'], 'source_dirty_diff_sha256': origin['source_dirty_diff_sha256'],
          'source_census_file': 'source-manifest.json', 'source_census_entries': len(selected),
          'source_census_sha256': source_sha, 'kernel_cargo_lock_sha256': lock,
          'source_capture_origin': str(capture / 'origin.json'), 'registry_packages': 128,
          'inputs_census_path': str(out / 'inputs-manifest.json'), 'inputs_census_sha256': inputs_sha,
          'inputs_files': len(items), 'inputs_bytes': sum(item['bytes'] for item in items),
          'archive_path': str(archive), 'archive_sha256': sha(archive), 'archive_bytes': archive.stat().st_size,
          'scope': 'frozen v2.0 kernel sources plus exact unchanged independently verified registry graph; no compiler, VM, host credentials or kernel boot claim'}
save('provenance.json', record)
print(json.dumps({'path': str(out), 'archive_sha256': record['archive_sha256'],
                  'inputs_census_sha256': inputs_sha, 'source_files': len(selected)}))
