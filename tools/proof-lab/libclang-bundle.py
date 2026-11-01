#!/usr/bin/env python3
"""Extract verified runtime libraries to private proof input, never install packages."""
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import stat
import sys
import tarfile
import tempfile

from run import ROOT, require, sha

os.umask(0o077)
lock_path = ROOT / 'moruna-env-501-7bd45bcd7fdb/package-resolution/package-lock.json'
require(sha(lock_path) == 'fbd4eeb24b327cd13e500ef5f44fc57a2fb4f8d3e3688185dabeecd300a64bbc', 'reviewed package lock differs')
lock = json.loads(lock_path.read_text())
cache = ROOT / 'package-cache/47557ae5b862b33ee072e8a18d240437/debs'
selected = {'libclang1-14', 'libllvm14', 'libz3-4', 'libobjc4', 'libgc1'}
compiler = sys.argv[1:] == ['--compiler']
require(not sys.argv[1:] or compiler, 'only explicit --compiler supported')
if compiler: selected |= {'clang', 'clang-14', 'libclang-common-14-dev', 'libclang-cpp14'}
packages = [p for p in lock['packages'] if p['package'] in selected]
require({p['package'] for p in packages} == selected, 'library closure differs')
out = Path(tempfile.mkdtemp(prefix='libclang-tools-', dir=ROOT))
tree = out / 'root'; tree.mkdir(mode=0o700)
items = {}

def ar_data(path):
    with path.open('rb') as file:
        require(file.read(8) == b'!<arch>\n', 'invalid deb archive')
        while header := file.read(60):
            require(len(header) == 60 and header[58:] == b'`\n', 'invalid ar header')
            name = header[:16].decode().strip().rstrip('/')
            size = int(header[48:58]); payload = file.read(size)
            require(len(payload) == size, 'truncated ar data')
            if size % 2: require(file.read(1) == b'\n', 'invalid ar alignment')
            if name.startswith('data.tar.'):
                require(name == 'data.tar.xz', 'unreviewed archive compression')
                return payload
    raise RuntimeError('deb data missing')

for package in packages:
    source = cache / Path(package['path']).name
    require(not source.is_symlink() and source.stat().st_size == package['bytes'] and sha(source) == package['sha256'], 'deb checksum differs')
    with tarfile.open(fileobj=io.BytesIO(ar_data(source)), mode='r:xz') as archive:
        for member in archive:
            rel = member.name.removeprefix('./')
            path = PurePosixPath(rel)
            require(not path.is_absolute() and '..' not in path.parts, 'deb path escape')
            library = rel.startswith('usr/lib/') and '.so' in path.name
            compiler_input = compiler and (rel.startswith('usr/lib/llvm-14/bin/') or
                rel.startswith('usr/lib/llvm-14/lib/clang/') or
                rel in {'usr/bin/clang','usr/bin/clang++','usr/bin/clang-14','usr/bin/clang++-14','usr/bin/clang-cpp-14'})
            if not (library or compiler_input):
                continue
            if member.isdir(): continue
            require(member.isfile() or member.issym(), 'unreviewed library archive member')
            dest = tree / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            require(not any(p.is_symlink() for p in dest.parents if p.is_relative_to(tree)), 'library parent symlink')
            require(not dest.exists() and not dest.is_symlink() and rel not in items, 'duplicate library path')
            if member.issym():
                target = member.linkname
                canonical = posixpath.normpath(target.lstrip('/') if target.startswith('/') else posixpath.join(str(path.parent), target))
                require(canonical != '..' and not canonical.startswith('../') and not canonical.startswith('/'), 'library link escape')
                target = posixpath.relpath(canonical, str(path.parent))
                dest.symlink_to(target)
                items[rel] = {'path': rel, 'type': 'symlink', 'target': target, 'package': package['package']}
            else:
                data = archive.extractfile(member).read()
                require(len(data) == member.size, 'short library data')
                dest.write_bytes(data); dest.chmod(0o555 if member.mode & 0o111 else 0o444)
                items[rel] = {'path': rel, 'type': 'file', 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(), 'package': package['package']}

for item in items.values():
    path = tree / item['path']
    require(path.resolve(strict=True).is_relative_to(tree), 'unresolved or escaping library link')
manifest = out / 'manifest.json'
manifest.write_text(json.dumps(sorted(items.values(), key=lambda i: i['path']), indent=2) + '\n')
info = tree.stat()
record = {'purpose': 'read-only native ARM Linux libclang runtime input; no package installation or host binary execution',
          'root': str(tree), 'device': info.st_dev, 'inode': info.st_ino, 'uid': os.getuid(),
          'destination': '/opt/moruna-proof-tools', 'manifest': str(manifest), 'manifest_sha256': sha(manifest),
          'package_lock': str(lock_path), 'package_lock_sha256': sha(lock_path),
          'packages': packages, 'files': len(items), 'bytes': sum(i.get('bytes', 0) for i in items.values()),
          'libclang_path': 'usr/lib/aarch64-linux-gnu',
          'library_paths': ['usr/lib/aarch64-linux-gnu', 'usr/lib/llvm-14/lib']}
if compiler: record['clang_executable']='usr/lib/llvm-14/bin/clang'
(out / 'bundle.json').write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps({'bundle': str(out / 'bundle.json'), 'files': record['files'], 'bytes': record['bytes'], 'manifest_sha256': record['manifest_sha256']}))
