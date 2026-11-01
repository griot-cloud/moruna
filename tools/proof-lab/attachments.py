"""Prepared exact private-cache attachment checks. No action occurs on import.

Use together with VM nonce/label/process/image ownership validation, never as its substitute.
Parser.swift432–546 maps bind to virtiofs; actual1.6.1 inspect encodes type as {virtiofs:{}}.
"""
import json
import os
from pathlib import Path
import re
import stat

ROOT = Path(__file__).resolve().parent


def private_directory(path):
    path = Path(path)
    if not path.is_absolute() or not path.is_relative_to(ROOT):
        raise ValueError('attachment outside the exact private Linux root')
    for part in [ROOT, *[ROOT.joinpath(*path.relative_to(ROOT).parts[:i]) for i in range(1, len(path.relative_to(ROOT).parts) + 1)]]:
        info = part.lstat()
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
            raise ValueError('attachment component is not an owned real directory')
    return path


def create_cache(nonce, lock_sha256, toolchain, target, profile, inputs_sha256=None):
    if not re.fullmatch(r'[0-9a-f]{32}', nonce) or not re.fullmatch(r'[0-9a-f]{64}', lock_sha256):
        raise ValueError('nonce and exact source-lock digest required')
    if inputs_sha256 is not None and not re.fullmatch(r'[0-9a-f]{64}', inputs_sha256):
        raise ValueError('invalid exact input-census digest')
    for value in (toolchain, target, profile):
        if not re.fullmatch(r'[A-Za-z0-9_-][A-Za-z0-9_.-]*', value) or '..' in value:
            raise ValueError('invalid explicit cache namespace component')
    for name in ('caches', 'cache-journals'):
        path = ROOT / name
        path.mkdir(mode=0o700, exist_ok=True)
        private_directory(path)
    parent = ROOT / 'caches' / nonce
    parent.mkdir(mode=0o700)  # exclusive; never adopt another run's cache
    path = parent
    namespace = (lock_sha256, *([inputs_sha256] if inputs_sha256 is not None else []), toolchain, target, profile, 'target')
    for name in namespace:
        path = path / name
        path.mkdir(mode=0o700)
    private_directory(path)
    info = path.stat()
    record = {'owner_nonce': nonce, 'path': str(path), 'device': info.st_dev, 'inode': info.st_ino,
              'source_lock_sha256': lock_sha256, 'toolchain': toolchain, 'target': target,
              'profile': profile, 'destination': '/proof/target', 'readonly': False}
    if inputs_sha256 is not None:
        record['inputs_census_sha256'] = inputs_sha256
    journal = ROOT / 'cache-journals' / (nonce + '.json')
    descriptor = os.open(journal, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as output:
        json.dump(record, output, indent=2)
    return record


def validate_cache(record, nonce):
    if record['owner_nonce'] != nonce or record.get('readonly') is not False or record['destination'] != '/proof/target':
        raise ValueError('target cache purpose/owner differs')
    path = private_directory(Path(record['path']))
    if (path.stat().st_dev, path.stat().st_ino) != (record['device'], record['inode']):
        raise ValueError('target cache inode changed')
    journal = ROOT / 'cache-journals' / (nonce + '.json')
    info = journal.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1:
        raise ValueError('private external cache journal ownership differs')
    if json.loads(journal.read_text()) != record:
        raise ValueError('external cache journal differs')


def flags(approved):
    result = []
    for item in approved:
        source = private_directory(Path(item['path']))
        # Inputs must be separately hash/provenance verified before appearing in approved.
        result += ['--mount', f'type=bind,source={source},target={item["destination"]}' + (',readonly' if item['readonly'] else '')]
    return result


def validate_inspect(configuration, approved):
    expected = []
    for item in approved:
        source = private_directory(Path(item['path']))
        expected.append({'source': str(source), 'destination': item['destination'],
                         'type': {'virtiofs': {}}, 'options': ['ro'] if item['readonly'] else []})
    actual = configuration.get('mounts', [])
    key = lambda value: (value['destination'], value['source'])
    if sorted(actual, key=key) != sorted(expected, key=key):
        raise ValueError('VM attachments differ from exact approved private mounts')
    if any(configuration.get(k) for k in ('publishedPorts', 'publishedSockets', 'ssh', 'capAdd', 'virtualization', 'rosetta')):
        raise ValueError('VM gained forbidden ports/sockets/SSH/capabilities')
    resources = configuration['resources']
    if resources['cpus'] != 2 or not 0 < resources['memoryInBytes'] <= 6 * 1024**3:
        raise ValueError('VM resources exceed the approved maximum')
