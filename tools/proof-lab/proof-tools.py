"""Validate exact read-only extracted runtime tool input before every proof stage."""
import json
import os
from pathlib import Path
import stat

from attachments import private_directory
from run import ROOT, require, sha


def load_bundle(raw):
    path = Path(raw).resolve(strict=True)
    require(path.is_relative_to(ROOT) and not path.is_symlink(), 'tool record outside owned proof root')
    info = path.stat()
    require(info.st_uid == os.getuid() and info.st_nlink == 1, 'tool record ownership differs')
    record = json.loads(path.read_text())
    root = private_directory(Path(record['root']))
    require((root.stat().st_dev, root.stat().st_ino, root.stat().st_uid) ==
            (record['device'], record['inode'], record['uid']), 'tool tree identity differs')
    manifest = Path(record['manifest'])
    require(manifest.parent == root.parent and not manifest.is_symlink() and sha(manifest) == record['manifest_sha256'], 'tool manifest differs')
    require(sha(Path(record['package_lock'])) == record['package_lock_sha256'], 'tool package lock differs')
    entries = json.loads(manifest.read_text()); expected = {i['path']: i for i in entries}
    require(len(entries) == record['files'] == len(expected), 'tool census count differs')
    actual = set()
    for path in root.rglob('*'):
        info = path.lstat()
        require(info.st_uid == os.getuid(), 'tool file owner differs')
        if stat.S_ISDIR(info.st_mode): continue
        rel = str(path.relative_to(root)); require(rel in expected, 'extra tool input')
        item = expected[rel]; actual.add(rel)
        if item['type'] == 'symlink':
            require(path.is_symlink() and os.readlink(path) == item['target'] and
                    path.resolve(strict=True).is_relative_to(root), 'tool link differs or escapes')
        else:
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and
                    info.st_size == item['bytes'] and sha(path) == item['sha256'], 'tool bytes differ')
    require(actual == expected.keys(), 'missing tool input')
    require(record['destination'] == '/opt/moruna-proof-tools', 'tool mount destination differs')
    return record
