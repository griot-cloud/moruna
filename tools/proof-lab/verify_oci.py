#!/usr/bin/env python3
"""Read-only OCI archive verification. Never extracts layer files or invokes a runtime."""
import argparse
import hashlib
import json
from pathlib import PurePosixPath
import re
import tarfile


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def verify(path, expected_manifest=None, platform=None):
    with tarfile.open(path, 'r:*') as archive:
        members = {}
        blobs = {}
        for member in archive:
            name = member.name.removeprefix('./')
            parts = PurePosixPath(name)
            require(not parts.is_absolute() and '..' not in parts.parts, 'archive path escape')
            if member.isdir():
                require(name.rstrip('/') in ('blobs', 'blobs/sha256'), 'unexpected directory')
                continue
            require(member.isfile(), 'OCI layout must contain regular files, never links/devices')
            require(name not in members, 'duplicate archive member')
            require(name in ('oci-layout', 'index.json') or re.fullmatch(r'blobs/sha256/[0-9a-f]{64}', name), 'unexpected OCI layout member')
            members[name] = member
            if name.startswith('blobs/'):
                digest = hashlib.sha256()
                stream = archive.extractfile(member)
                require(stream is not None, 'unreadable blob')
                for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                    digest.update(chunk)
                require(digest.hexdigest() == parts.name, 'blob bytes differ from addressed SHA256')
                blobs['sha256:' + parts.name] = member.size
        def read_json(name):
            require(name in members and members[name].size <= 16 * 1024 * 1024, 'missing/oversized OCI metadata')
            return json.load(archive.extractfile(members[name]))
        require(read_json('oci-layout') == {'imageLayoutVersion': '1.0.0'}, 'unknown OCI layout version')
        index = read_json('index.json')
        require(index.get('schemaVersion') == 2, 'unknown index schema')
        seen = set()
        found = []
        def walk(descriptor):
            digest = descriptor['digest']
            require(re.fullmatch(r'sha256:[0-9a-f]{64}', digest), 'non-SHA256 descriptor')
            require(blobs.get(digest) == descriptor['size'], 'descriptor blob/size missing or mismatched')
            if digest in seen:
                return
            seen.add(digest)
            media = descriptor['mediaType']
            if media in ('application/vnd.oci.image.index.v1+json', 'application/vnd.docker.distribution.manifest.list.v2+json'):
                for child in read_json('blobs/sha256/' + digest[7:])['manifests']:
                    walk(child)
            elif media in ('application/vnd.oci.image.manifest.v1+json', 'application/vnd.docker.distribution.manifest.v2+json'):
                manifest = read_json('blobs/sha256/' + digest[7:])
                require(manifest.get('schemaVersion') == 2, 'unknown manifest schema')
                config = manifest['config']
                for child in [config, *manifest['layers']]:
                    child_digest = child['digest']
                    require(re.fullmatch(r'sha256:[0-9a-f]{64}', child_digest), 'non-SHA256 payload')
                    require(blobs.get(child_digest) == child['size'], 'config/layer missing or mismatched')
                configuration = read_json('blobs/sha256/' + config['digest'][7:])
                found.append({'manifest': digest, 'platform': configuration.get('os', '') + '/' + configuration.get('architecture', ''),
                              'config': config['digest'], 'layers': [item['digest'] for item in manifest['layers']]})
            else:
                raise ValueError('unsupported index child media type: ' + media)
        for descriptor in index['manifests']:
            walk(descriptor)
        require(found, 'no image manifest')
        if expected_manifest:
            require(any(item['manifest'] == expected_manifest and (not platform or item['platform'] == platform) for item in found), 'pinned platform manifest not found')
        elif platform:
            require(any(item['platform'] == platform for item in found), 'requested platform absent')
        return {'archive_verified': True, 'images': found, 'blob_count': len(blobs),
                'pin_enforced': bool(expected_manifest), 'layer_files_extracted': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive')
    parser.add_argument('--expected-manifest')
    parser.add_argument('--platform', choices=('linux/arm64', 'linux/amd64'))
    options = parser.parse_args()
    print(json.dumps(verify(options.archive, options.expected_manifest, options.platform), indent=2))


if __name__ == '__main__':
    main()
