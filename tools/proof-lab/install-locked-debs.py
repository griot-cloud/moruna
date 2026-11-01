#!/usr/bin/env python3
"""Prepared offline provisioning step inside the NEW nonce-owned Linux VM only.

LOCK: {"packages": [{"path": "/inputs/debs/NAME.deb", "sha256": "...",
"package": "...", "version": "...", "architecture": "arm64|amd64|all"}]}
Actual package versions/archives must be resolved and reviewed after inventory, never guessed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('nonce')
    parser.add_argument('lock')
    options = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'aarch64' or Path('/run/moruna-proof-owner').read_text().strip() != options.nonce:
        raise RuntimeError('new owned Linux provisioning VM marker required')
    lock = Path(options.lock)
    packages = json.loads(lock.read_text())['packages']
    if not packages:
        raise RuntimeError('empty package lock is not a provisioning proof')
    paths = []
    for package in packages:
        path = Path(package['path'])
        if not path.is_absolute() or not path.resolve().is_relative_to('/inputs/debs') or path.is_symlink() or not path.is_file():
            raise RuntimeError('package path outside exact private package inputs')
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != package['sha256']:
            raise RuntimeError('package archive SHA mismatch')
        for field, key in [('Package', 'package'), ('Version', 'version'), ('Architecture', 'architecture')]:
            observed = subprocess.check_output(['dpkg-deb', '--field', str(path), field], text=True).strip()
            if observed != package[key]:
                raise RuntimeError('package control metadata differs: ' + field)
        paths.append(str(path))
    # No apt update, repository edits, network downloads, force-depends or repair fallback.
    subprocess.run(['apt-get', '--no-download', '--yes', '--no-install-recommends', 'install', *paths], check=True,
                   env={**os.environ, 'DEBIAN_FRONTEND': 'noninteractive'})
    result = subprocess.check_output(['dpkg-query', '-W', '-f=${binary:Package} ${Version} ${Architecture}\n'], text=True)
    Path('/proof/installed-packages.txt').write_text(result)
    Path('/proof/package-lock.sha256').write_text(hashlib.sha256(lock.read_bytes()).hexdigest() + '\n')
    for package in packages:
        query = package['package'] + (':' + package['architecture'] if package['architecture'] != 'all' else '')
        observed = subprocess.check_output(['dpkg-query', '-W', '-f=${Version}', query], text=True).strip()
        if observed != package['version']:
            raise RuntimeError('installed package readback differs: ' + query)
    print('MORUNA_LOCKED_PACKAGES_INSTALLED ' + options.nonce)


if __name__ == '__main__':
    main()
