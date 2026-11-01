#!/usr/bin/env python3
"""Guest-only canonical KD3 preparation and actual Linux execution, never a VM proof."""
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time
import zipfile


def require(ok, why):
    if not ok:
        raise RuntimeError(why)


def sha(path):
    with Path(path).open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def census(root):
    rows = []
    total = 0
    for path in sorted(root.rglob('*')):
        if path.is_dir():
            require(not path.is_symlink(), 'bundle directory symlink refused')
            continue
        info = path.lstat()
        relative = path.relative_to(root).as_posix()
        image_layer = relative.startswith('layers/stack-') and relative.endswith('.img')
        limit = 2 * 1024**3 if image_layer else 512 * 1024**2
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= limit,
                'bundle file type/size differs')
        total += info.st_size
        require(total <= 6 * 1024**3 and len(rows) < 50000, 'prepared bundle aggregate bound')
        rows.append({'path': path.relative_to(root).as_posix(), 'bytes': info.st_size,
                     'mode': stat.S_IMODE(info.st_mode), 'sha256': sha(path)})
    return rows


def wheel(path, destination):
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        require(len(entries) < 10000 and sum(x.file_size for x in entries) <= 512 * 1024**2,
                'official wheel bounds')
        for entry in entries:
            name = Path(entry.filename)
            require(not name.is_absolute() and '..' not in name.parts
                    and not stat.S_ISLNK(entry.external_attr >> 16), 'wheel path refused')
        archive.extractall(destination)


def main():
    token, seconds = sys.argv[1:]
    require(len(token) == 32 and all(c in '0123456789abcdef' for c in token), 'owner nonce invalid')
    deadline = time.monotonic() + int(seconds)
    config = json.loads(Path('/tmp/kd3-prepare-inputs.json').read_text())
    out = Path('/proof/target/kd3-prepare') / token
    out.mkdir(parents=True, mode=0o700)
    logs = out / 'logs'
    logs.mkdir()
    report = {'version': 1, 'passed': False, 'scope': 'actual Linux41 processes/46 phase receipts; native jobs NOT executed',
              'commands': [], 'input_sha256': config['input_sha256']}

    def save():
        (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')

    def call(args, name, env=None):
        process = None
        log = logs / (name + '.log')
        try:
            require(time.monotonic() < deadline, 'preparation active deadline exhausted')
            with log.open('xb') as stream:
                process = subprocess.Popen([str(x) for x in args], stdout=stream, stderr=subprocess.STDOUT,
                                           env=env, start_new_session=True)
                while process.poll() is None:
                    require(time.monotonic() < deadline and log.stat().st_size <= 32 * 1024**2,
                            'preparation deadline/log bound exceeded')
                    require(shutil.disk_usage('/proof').free >= 6 * 1024**3, 'hard6GiB disk guard')
                    time.sleep(.1)
            require(process.returncode == 0, 'actual preparation command failed: ' + name)
            return log.read_text().strip()
        finally:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            report['commands'].append({'name': name, 'argv': [str(x) for x in args],
                                       'exit': process.returncode if process else None,
                                       'log': 'logs/' + log.name, 'sha256': sha(log) if log.exists() else None})
            save()

    cgroup = Path('/sys/fs/cgroup') / ('moruna-kd3-' + token)
    try:
        status = Path('/proc/self/status').read_text()
        effective = int(next(line.split()[1] for line in status.splitlines() if line.startswith('CapEff:')), 16)
        require(effective & (1 << 21), 'actual CAP_SYS_ADMIN missing for sandbox namespace setup')
        report['preflight'] = {'effective_capabilities_hex': hex(effective), 'required_added_capability': 'CAP_SYS_ADMIN'}
        call(['/usr/bin/unshare', '--mount', '/bin/true'], 'actual-mount-namespace')
        # This is the actual sandbox hierarchy, not an isolation fallback.
        require(Path('/sys/fs/cgroup/cgroup.controllers').is_file(), 'real cgroup2 hierarchy required')
        controllers = Path('/sys/fs/cgroup/cgroup.controllers').read_text().split()
        require({'cpu', 'memory'} <= set(controllers), 'real CPU/memory controllers required')
        # This namespace exposes a delegated parent, not the exempt true root.
        # Move only its observed setup/init PIDs into our owned host leaf before
        # enabling domain controllers. The job hierarchy must remain empty.
        parent = Path('/sys/fs/cgroup')
        host_leaf = parent / ('moruna-kd3-host-' + token)
        host_leaf.mkdir(mode=0o700)
        delegation = {'parent': str(parent), 'self_cgroup_before': Path('/proc/self/cgroup').read_text(),
                      'controllers_before': (parent / 'cgroup.controllers').read_text(),
                      'subtree_before': (parent / 'cgroup.subtree_control').read_text(),
                      'host_leaf': str(host_leaf), 'snapshots': [], 'moves': [],
                      'host_leaf_lifetime': 'retained until actual owned VM teardown'}
        report['delegation'] = delegation
        save()
        for _ in range(8):
            pids = [int(value) for value in (parent / 'cgroup.procs').read_text().split()]
            require(len(pids) <= 256 and all(pid > 0 for pid in pids), 'bounded visible delegated-parent PIDs required')
            delegation['snapshots'].append(pids)
            save()
            if not pids:
                break
            for pid in pids:
                try:
                    (host_leaf / 'cgroup.procs').write_text(str(pid))
                    delegation['moves'].append({'pid': pid, 'outcome': 'moved'})
                except OSError as error:
                    delegation['moves'].append({'pid': pid, 'errno': error.errno, 'outcome': 'refused'})
                    save()
                    if error.errno != errno.ESRCH:
                        raise
                    # Only the real exited-process race is tolerated; other
                    # accounting/permission/write failures stay explicit.
                    delegation['moves'][-1]['outcome'] = 'exited-before-move'
                save()
        require(not (parent / 'cgroup.procs').read_text().strip(), 'delegated parent still has internal processes')
        (parent / 'cgroup.subtree_control').write_text('+cpu +memory')
        cgroup.mkdir(mode=0o700)
        (cgroup / 'cgroup.subtree_control').write_text('+cpu +memory')
        delegation.update(parent_procs_after=(parent / 'cgroup.procs').read_text(),
                          host_procs_after=(host_leaf / 'cgroup.procs').read_text(),
                          subtree_after=(parent / 'cgroup.subtree_control').read_text(),
                          job_procs_after=(cgroup / 'cgroup.procs').read_text(),
                          job_subtree_after=(cgroup / 'cgroup.subtree_control').read_text(),
                          self_cgroup_after=Path('/proc/self/cgroup').read_text())
        require(not delegation['parent_procs_after'].strip() and not delegation['job_procs_after'].strip(),
                'real domain-controller parents must remain empty')
        save()
        spec = importlib.util.spec_from_file_location('kd3_producer', '/tmp/kd3-fixture-producer.py')
        producer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(producer)
        work = Path('/proof/kd3-prepare-work')
        work.mkdir(mode=0o700)
        source = Path('/proof/source')
        python_bundle = config['python']
        pins = {}
        for line in (source / 'guest/pins.env').read_text().splitlines():
            if line and not line.startswith('#') and '=' in line:
                key, value = line.split('=', 1)
                pins[key] = value.strip().strip('"')
        require(config['yaml']['sha256'] in (source / 'uv.lock').read_text(), 'PyYAML is not locked by actual source')
        homes = {}
        for arch, pykey, pakey in (('arm', 'python_arm', 'pyarrow_arm'), ('native', 'python', 'pyarrow')):
            base = work / arch
            base.mkdir()
            py, arrow = python_bundle[pykey], python_bundle[pakey]
            pin_arch = 'aarch64' if arch == 'arm' else 'x86_64'
            require(sha(py['path']) == py['sha256'] == pins['PYTHON_STD_SHA256_' + pin_arch]
                    and sha(arrow['path']) == arrow['sha256'] == pins['PYARROW_STD_SHA256_' + pin_arch],
                    'actual source-pinned Python/PyArrow changed')
            producer.unpack_python(Path(py['path']), base)
            home = base / 'python'
            site = home / 'lib/python3.14/site-packages'
            site.mkdir(parents=True, exist_ok=True)
            wheel(arrow['path'], site)
            shutil.copytree(source / 'python/moruna', site / 'moruna')
            shutil.copy2(config[arch]['_core.so'], site / 'moruna/_core.so')
            homes[arch] = home
        yaml = config['yaml']
        require(sha(yaml['path']) == yaml['sha256'], 'actual pinned PyYAML changed')
        wheel(yaml['path'], homes['arm'] / 'lib/python3.14/site-packages')
        env = os.environ.copy()
        env['PYTHONHOME'] = str(homes['arm'])
        env['LD_LIBRARY_PATH'] = str(homes['arm'] / 'lib') + ':' + env.get('LD_LIBRARY_PATH', '')
        env.pop('MORUNA_KD3_PROOF_NONCE', None)
        python = homes['arm'] / 'bin/python3'
        call([python, '-I', '-B', '-c', 'import moruna,pyarrow,yaml; print(moruna.__version__,pyarrow.__version__,yaml.__version__)'], 'actual-module-import', env)
        author = source / 'python/tests/fixtures/kd3_cases.py'
        projects, packages = work / 'projects', work / 'packages'
        packages.mkdir()
        call([python, author, 'projects', '--root', projects], 'canonical-projects', env)
        for name in ('wasm', 'pex', 'pex-assets'):
            args = [python, '-m', 'moruna._package', projects / name, '--out', packages, '--no-index']
            if name == 'wasm':
                args += ['--wasm', source / 'python/tests/fixtures/echo.component.wasm']
            call(args, 'canonical-package-' + name, env)
        contracts = work / 'contracts.json'
        call([config['arm']['moruna-kd3-fixture'], 'seed-contract', contracts], 'canonical-contracts', env)
        assets = Path('/kd3')
        call([python, author, 'assets', '--root', assets, '--wasm', packages / 'echo.wasm',
              '--pex', packages / 'identity.pex', '--pex-assets', packages / 'assets_identity.pex',
              '--contracts', contracts], 'actual-assets', env)
        # Neither cases nor their derived-image reference enter the image whose digest they name.
        programs = work / 'programs'
        (programs / 'moruna/bin').mkdir(parents=True)
        shutil.copy2(config['native']['moruna-kd3-fixture'], programs / 'moruna/bin/moruna-kd3-fixture')
        (programs / 'opt').mkdir()
        shutil.copytree(homes['native'], programs / 'opt/python-std', symlinks=True)
        shutil.copytree(assets, programs / 'kd3', symlinks=True)
        echo = source / 'crates/moruna-image/tests/fixtures/arrow_echo.rs'
        report['canonical_arrow_echo_source_sha256'] = sha(echo)
        (programs / 'kc1/bin').mkdir(parents=True)
        shutil.copy2(config['native_echo'], programs / 'kc1/bin/arrow_echo')
        layer = config['layer']
        native_layout = Path('/kd3-image-layout')
        native_digest = call([layer, 'derive', config['oci']['native']['root'], config['oci']['native']['reference'],
                              programs, 'kd3-native', native_layout, '/moruna/bin/moruna-kd3-fixture'], 'derive-native', env)
        arm_programs = work / 'arm-programs'
        (arm_programs / 'kc1/bin').mkdir(parents=True)
        shutil.copy2(config['arm']['arrow_echo'], arm_programs / 'kc1/bin/arrow_echo')
        arm_layout = Path('/kd3-linux-image-layout')
        arm_digest = call([layer, 'derive', config['oci']['arm']['root'], config['oci']['arm']['reference'],
                           arm_programs, 'kd3-linux', arm_layout, '/kc1/bin/arrow_echo'], 'derive-arm', env)
        bundle = out / 'bundle'
        bundle.mkdir()
        stack = bundle / 'layers'
        call([layer, 'stack', native_layout, native_digest, stack, '2'], 'canonical-native-stack', env)
        authored = work / 'authored'
        call([python, author, 'cases', '--root', assets, '--output', authored,
              '--image', native_digest, '--image-layout', native_layout,
              '--linux-image', arm_digest, '--linux-image-layout', arm_layout,
              '--sandbox-python', python, '--sandbox-cgroup', cgroup], 'actual-cases', env)
        # Actual Linux isolation failures remain failures; never execute outside the sandbox.
        linux = out / 'linux'
        call([python, author, 'linux', '--prepared', authored, '--assets', assets,
              '--fixture', config['arm']['moruna-kd3-fixture'], '--output', linux,
              '--seconds', max(1, min(180, int(deadline - time.monotonic())))], 'actual-linux41', env)
        for name in ('cases', 'jobs', 'commands'):
            shutil.copytree(authored / name, bundle / name)
        shutil.copy2(authored / 'cells.json', bundle / 'cells.json')
        shutil.copy2(assets / 'matrix.json', bundle / 'matrix.json')
        shutil.copytree(linux / 'results', bundle / 'linux-results')
        (bundle / 'host').mkdir()
        shutil.copy2(config['arm']['kd3-fixture-boot'], bundle / 'host/kd3-fixture-boot')
        shutil.copy2(source / 'tools/kd3-matrix.py', bundle / 'kd3-matrix.py')
        rows = census(bundle)
        (out / 'bundle-manifest.json').write_text(json.dumps(rows, indent=2) + '\n')
        report.update(passed=True, linux_processes=41, linux_phase_receipts=46,
                      native_executed=0, native_digest=native_digest, linux_digest=arm_digest,
                      bundle_files=len(rows), bundle_bytes=sum(x['bytes'] for x in rows))
        save()
    except BaseException as error:
        report['error'] = type(error).__name__ + ': ' + str(error)
        save()
        raise
    finally:
        # Setup/init remain in their leaf until the runner tears down THIS VM.
        # Moving them back would violate the enabled parent's domain rules.
        delegation = report.get('delegation')
        try:
            if cgroup.exists():
                if delegation is not None:
                    delegation['job_procs_at_cleanup'] = (cgroup / 'cgroup.procs').read_text()
                cgroup.rmdir()
                if delegation is not None:
                    delegation['job_parent_removed'] = True
            if delegation is not None:
                leaf = Path(delegation['host_leaf'])
                delegation['host_procs_before_vm_teardown'] = (leaf / 'cgroup.procs').read_text()
                delegation['parent_procs_before_vm_teardown'] = Path('/sys/fs/cgroup/cgroup.procs').read_text()
                delegation['host_leaf_removed_by_helper'] = False
                save()
        except BaseException as error:
            report['cgroup_cleanup_error'] = type(error).__name__ + ': ' + str(error)
            save()
            raise



def host_inputs(path, source_inputs, proof_root):
    """Read-only exact actual producer/artifact closure, mounted only under private proof root."""
    from attachments import private_directory
    def private_file(value):
        value = Path(value)
        private_directory(value.parent)
        z = value.lstat()
        require(stat.S_ISREG(z.st_mode) and z.st_uid == os.getuid() and z.st_nlink == 1,
                'private preparation file type/owner differs')
        return value
    path = private_file(path)
    v = json.loads(path.read_text())
    require(v.get('version') == 1, 'KD3 preparation input version differs')
    mounts, fingerprints, mapped = [], [], {}

    def file(p):
        p = private_file(p)
        require(p.is_relative_to(proof_root), 'preparation attachment outside owned proof root')
        z = p.stat()
        fingerprints.append((p, (z.st_dev, z.st_ino, z.st_uid, z.st_size, z.st_mtime_ns)))
        return p

    def mount(root):
        root = private_directory(Path(root))
        if root not in mapped:
            mapped[root] = '/inputs/kd3-prepare/' + str(len(mapped))
            mounts.append({'path': str(root), 'destination': mapped[root], 'readonly': True})
        return mapped[root]

    def artifact(p, expected):
        p = file(p)
        require(p.stat().st_size == expected['bytes'] and sha(p) == expected['sha256'], 'actual preparation artifact differs')
        return mount(p.parent) + '/' + p.name

    config = {'input_sha256': sha(path), 'oci': {}}
    file(path)
    for role, target, names in (('arm', 'aarch64-unknown-linux-gnu', {'moruna-kd3-fixture', '_core.so', 'kd3-fixture-boot', 'arrow_echo'}),
                                ('native', 'x86_64-unknown-linux-gnu', {'moruna-kd3-fixture', '_core.so'})):
        ref = v['gnu' if role == 'native' else role]
        summary, manifest = file(ref['summary']), file(ref['manifest'])
        a, m = json.loads(summary.read_text()), json.loads(manifest.read_text())
        phase = 'kd3-fixture-tools-' + ('gnu' if role == 'native' else role)
        require(a.get('stage_passed') is True and a.get('cleanup') is True and a.get('guest_exit') == 0
                and a.get('phase') == phase and m.get('passed') is True and m.get('target') == target,
                'actual successful fixture producer required')
        collection = file(summary.parent / 'kd3-fixture-artifacts.json')
        c = json.loads(collection.read_text())
        require(c.get('collected') is True and 'error' not in c and c.get('actual') == m,
                'post-cleanup fixture custody required')
        require({r['name'] for r in m['files']} == names and len(m['files']) == len(names), 'exact ARM4/GNU2 required')
        config[role] = {r['name']: artifact(manifest.parent / r['name'], r) for r in m['files']}
    for role, wanted in (('host', 'moruna-layer'), ('personality', 'arrow_echo')):
        ref = v[role]
        summary, manifest = file(ref['summary']), file(ref['manifest'])
        a, m = json.loads(summary.read_text()), json.loads(manifest.read_text())
        require(a.get('stage_passed') is True and a.get('cleanup') is True and a.get('guest_exit') == 0
                and (role != 'personality' or m.get('passed') is True)
                and a.get('phase') == ('boot-host-tools' if role == 'host' else 'personality-artifacts'),
                'actual layer/echo producer required')
        entries = m['files'] if role == 'host' else m['artifacts']
        row = next(r for r in entries if r['name'] == wanted)
        artifact_root = private_directory(Path(ref.get('root', manifest.parent)))
        config['layer' if role == 'host' else 'native_echo'] = artifact(artifact_root / wanted, row)
    receipt = file(v['python_bundle'])
    python = json.loads(receipt.read_text())
    config['python'] = {}
    for key in ('python', 'python_arm', 'pyarrow', 'pyarrow_arm'):
        row = python[key]
        require(row.get('verified') is True, 'official Python receipt must be verified')
        config['python'][key] = {**row, 'path': artifact(row['path'], row)}
    yaml = json.loads(file(v['yaml_bundle']).read_text())
    require(yaml.get('verified') is True and yaml['package'] == 'pyyaml' and yaml['package_version'] == '6.0.3',
            'actual locked PyYAML required')
    config['yaml'] = {**yaml, 'path': artifact(yaml['path'], yaml)}
    for role, arch in (('arm', 'arm64'), ('native', 'amd64')):
        ref = v['oci'][role]
        root = private_directory(Path(ref['root']))
        require(not root.is_symlink(), 'OCI root symlink refused')
        rows = census(root)
        for row in rows:
            file(root / row['path'])
        for row in rows:
            if row['path'].startswith('blobs/sha256/'):
                require(Path(row['path']).name == row['sha256'], 'OCI actual content digest differs')
        def blob(digest):
            require(isinstance(digest, str) and digest.startswith('sha256:') and len(digest) == 71
                    and all(c in '0123456789abcdef' for c in digest[7:]), 'canonical OCI digest required')
            return root / 'blobs/sha256' / digest[7:]
        index = json.loads((root / 'index.json').read_text())
        seen = set()
        def reachable(document, depth=0):
            require(depth <= 8 and len(seen) <= 512, 'OCI index traversal bound')
            for entry in document.get('manifests', []):
                digest = entry['digest']
                if digest == ref['reference']:
                    return True
                if digest not in seen and entry.get('mediaType') in (
                        'application/vnd.oci.image.index.v1+json',
                        'application/vnd.docker.distribution.manifest.list.v2+json'):
                    seen.add(digest)
                    child = json.loads(blob(digest).read_text())
                    if reachable(child, depth + 1):
                        return True
            return False
        require(reachable(index), 'actual selected OCI manifest is not reachable from layout index')
        selected = json.loads(blob(ref['reference']).read_text())
        actual_config = json.loads(blob(selected['config']['digest']).read_text())
        require(actual_config['architecture'] == arch and actual_config['os'] == 'linux', 'actual OCI config architecture differs')
        config['oci'][role] = {'root': mount(root), 'reference': ref['reference']}
    manifest = file(v['source_manifest'])
    require(sha(manifest) == source_inputs['inputs_census_sha256'], 'shipping source census differs')
    require(v['oci']['native']['reference'] != v['oci']['arm']['reference'], 'different actual architecture images required')
    return config, mounts, fingerprints, v

if __name__ == '__main__':
    main()
