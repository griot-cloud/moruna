#!/usr/bin/env python3
"""Prepared local-only init OCI save; run only under the coordinator's bounded init-save stage."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import runpy
import secrets
import subprocess

ROOT = Path(__file__).resolve().parent
CLI = '/opt/homebrew/bin/container'
REFERENCE = 'ghcr.io/apple/containerization/vminit:0.53.0'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--stage-owner-prefix', default='codex moruna init-save-')
    options = parser.parse_args()
    if not options.run:
        print(json.dumps({'prepared_only': True, 'reference': REFERENCE, 'platform': 'linux/arm64', 'no_export_executed': True}))
        return 0
    os.umask(0o077)
    lock = Path.home() / '.vm-lock'
    owner = lock / 'owner'
    if lock.is_symlink() or not lock.is_dir() or owner.is_symlink() or not owner.read_text().startswith(options.stage_owner_prefix):
        raise RuntimeError('coordinator init-save stage must own the shared lock')
    identity = (lock.stat().st_dev, lock.stat().st_ino, owner.read_text())
    def guard():
        if identity != (lock.stat().st_dev, lock.stat().st_ino, owner.read_text()):
            raise RuntimeError('shared lock ownership changed')
    directory = ROOT / ('init-save-' + secrets.token_hex(12))
    directory.mkdir(mode=0o700)
    environment = {'HOME': str(Path.home()), 'PATH': '/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin', 'LANG': 'C', 'LC_ALL': 'C'}
    with (directory / 'cached-inspect.log').open('xb') as output:
        subprocess.run([CLI, 'image', 'inspect', REFERENCE], stdout=output, stderr=subprocess.STDOUT, env=environment, check=True, timeout=15)
    guard()
    # ImageSave calls ClientImage.get/save; neither pulls, starts services nor filters init.
    # No --pull/build/load/commit command, no global image mutation, and no OCI layer extraction.
    archive = directory / 'init.oci.tar'
    with (directory / 'save.log').open('xb') as output:
        subprocess.run([CLI, 'image', 'save', '--platform', 'linux/arm64', '--output', str(archive), REFERENCE], stdout=output, stderr=subprocess.STDOUT, env=environment, check=True, timeout=80)
    guard()
    archive.chmod(0o600)
    verifier = runpy.run_path(str(ROOT / 'verify_oci.py'), run_name='oci_verifier')
    result = verifier['verify'](archive, platform='linux/arm64')
    digest = hashlib.sha256()
    with archive.open('rb') as data:
        for chunk in iter(lambda: data.read(1024 * 1024), b''):
            digest.update(chunk)
    result.update(reference=REFERENCE, archive_sha256=digest.hexdigest(), observed_only=True,
                  note='First digest observation, not a previously established immutable pin; coordinator reviews/promotes before VM use.')
    (directory / 'observed-init.json').write_text(json.dumps(result, indent=2) + '\n')
    guard()
    print(json.dumps({'evidence': str(directory), 'verified': True, 'images': result['images'], 'observed_only': True}))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
