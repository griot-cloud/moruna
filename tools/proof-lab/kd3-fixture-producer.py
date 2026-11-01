#!/usr/bin/env python3
"""Guest-only real KD3 Python fixture/library producer; not an execution proof."""
import ast
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import time
import zipfile


def require(ok, why):
    if not ok:
        raise RuntimeError(why)


def sha(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def extract_fresh(archive, destination, members, prefix):
    """Portable data-only extraction: fresh prefix, confined links, no special files.

    Files/directories are created before links; no archive-controlled symlink can
    become a write parent. Hard links resolve only to an actual regular member.
    """
    destination = destination.resolve(strict=True)
    require(destination.is_dir(), 'archive destination is not a directory')
    top = destination / prefix
    require(not top.exists() and not top.is_symlink(), 'archive output prefix must be fresh')
    table = {}
    for member in members:
        name = member.name.rstrip('/') if member.isdir() else member.name
        require(name and '\x00' not in name and '\\' not in name and not name.startswith('/')
                and all(part not in ('', '.', '..') for part in name.split('/'))
                and name.split('/')[0] == prefix, 'archive member path escape')
        require(name not in table, 'duplicate archive member')
        require(member.isdir() or member.isfile() or member.issym() or member.islnk(), 'special archive member refused')
        table[name] = member

    def resolved(name):
        def walk(parts, trail=()):
            current = []
            for index, part in enumerate(parts):
                if part in ('', '.'):
                    continue
                if part == '..':
                    require(len(current) > 1, 'archive link walks outside prefix')
                    current.pop()
                    continue
                current.append(part)
                require(current[0] == prefix, 'archive link leaves prefix')
                candidate = '/'.join(current)
                member = table.get(candidate)
                if member is not None and (member.issym() or member.islnk()):
                    require(len(trail) < 64 and candidate not in trail, 'archive link cycle')
                    value = member.linkname
                    require(value and '\x00' not in value and '\\' not in value and not value.startswith('/'), 'absolute/invalid archive link')
                    base = current[:-1] if member.issym() else []
                    return walk([*base, *value.split('/'), *parts[index + 1:]], (*trail, candidate))
            return '/'.join(current)
        return walk(name.split('/'))

    directories = {prefix}
    for name, member in table.items():
        parts = name.split('/')
        for index in range(1, len(parts)):
            parent = '/'.join(parts[:index])
            require(parent not in table or table[parent].isdir(), 'archive write parent is not a directory')
            directories.add(parent)
        if member.isdir():
            directories.add(name)
        if member.issym() or member.islnk():
            final = resolved(name)
            require(final == prefix or final.startswith(prefix + '/'), 'archive link resolution escapes')
            if member.islnk():
                require(final in table and table[final].isfile(), 'hard link must name an archive regular file')
    for name in sorted(directories, key=lambda value: (value.count('/'), value)):
        (destination / name).mkdir(mode=0o700)
    for name, member in table.items():
        if member.isfile():
            with archive.extractfile(member) as source, (destination / name).open('xb') as output:
                shutil.copyfileobj(source, output, length=1024 * 1024)
            require((destination / name).stat().st_size == member.size, 'archive regular size differs')
            (destination / name).chmod(member.mode & 0o777)
    for name, member in table.items():
        if member.islnk():
            os.link(destination / resolved(name), destination / name)
    for name, member in table.items():
        if member.issym():
            os.symlink(member.linkname, destination / name)
    for name in sorted(directories, key=lambda value: value.count('/'), reverse=True):
        member = table.get(name)
        (destination / name).chmod((member.mode & 0o777) if member is not None else 0o755)


def unpack_python(path, destination):
    with tarfile.open(path) as archive:
        total = 0
        members = archive.getmembers()
        require(len(members) < 50000, 'Python archive member bound')
        for member in members:
            p = Path(member.name)
            require(not p.is_absolute() and '..' not in p.parts and p.parts[0] == 'python', 'Python archive path escape')
            total += member.size
        require(total <= 1024**3, 'Python archive byte bound')
        extract_fresh(archive, destination, members, 'python')


def main():
    token, target, seconds = sys.argv[1:]
    seconds = int(seconds)
    require(sys.platform == 'linux' and target in ('aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu'), 'actual Linux GNU target required')
    require(len(token) == 32 and all(c in '0123456789abcdef' for c in token), 'invalid owner nonce')
    deadline = time.monotonic() + seconds
    out = Path('/proof/target/kd3-fixture-tools') / token
    out.mkdir(parents=True, mode=0o700)
    work = Path('/proof/kd3-fixture-work')
    work.mkdir(mode=0o700)
    report = {'version': 1, 'passed': False, 'target': target, 'toolchain': '1.98.1',
              'profile': 'dev-debug0', 'jobs': 1, 'scope': 'actual fixture/library artifact build only; no job/VM execution or performance claim',
              'commands': [], 'files': []}

    def save():
        temp = out / 'manifest.new'
        temp.write_text(json.dumps(report, indent=2) + '\n')
        temp.replace(out / 'manifest.json')

    env = dict(os.environ)
    env.update(CARGO_BUILD_JOBS='1', CARGO_NET_OFFLINE='true', RUSTFLAGS='--cfg polyval_force_soft')
    env.pop('CARGO_ENCODED_RUSTFLAGS', None)

    def call(argv, name):
        log = out / (name + '.log')
        process = None
        try:
            with log.open('xb') as output:
                process = subprocess.Popen([str(x) for x in argv], cwd='/proof/source', env=env,
                                           stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
                while process.poll() is None:
                    require(time.monotonic() < deadline, 'fixture producer deadline expired')
                    require(log.stat().st_size <= 32 * 1024**2, 'fixture build log bound')
                    time.sleep(.1)
            require(process.returncode == 0, 'actual command failed: ' + name)
        finally:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)
            report['commands'].append({'name': name, 'argv': [str(x) for x in argv],
                                       'exit': None if process is None else process.returncode,
                                       'log': log.name, 'sha256': sha(log) if log.exists() else None})
            save()
        return log.read_text()

    try:
        bundle = json.loads(Path('/tmp/kd3-python-bundle.json').read_text())
        arch = 'x86_64' if target.startswith('x86_64') else 'aarch64'
        python = bundle['python'] if arch == 'x86_64' else bundle['python_arm']
        arrow = bundle['pyarrow'] if arch == 'x86_64' else bundle['pyarrow_arm']
        pins = {}
        for line in Path('/proof/source/guest/pins.env').read_text().splitlines():
            if line and not line.startswith('#') and '=' in line:
                key, value = line.split('=', 1)
                pins[key] = value.strip().strip('"')
        python_tar = Path('/inputs/kd3-python') / Path(python['path']).name
        arrow_wheel = Path('/inputs/kd3-python') / Path(arrow['path']).name
        require(sha(python_tar) == python['sha256'] == pins['PYTHON_STD_SHA256_' + arch], 'official Python pin differs')
        require(sha(arrow_wheel) == arrow['sha256'] == pins['PYARROW_STD_SHA256_' + arch], 'official PyArrow pin differs')
        unpack_python(python_tar, work)
        home = work / 'python'
        site = home / 'lib/python3.14/site-packages'
        site.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(arrow_wheel) as archive:
            members = archive.infolist()
            require(len(members) < 10000 and sum(m.file_size for m in members) <= 512 * 1024**2, 'PyArrow wheel bound')
            for m in members:
                path = Path(m.filename)
                require(not path.is_absolute() and '..' not in path.parts and not stat.S_ISLNK(m.external_attr >> 16), 'PyArrow wheel path escape')
            archive.extractall(site)
        env['LD_LIBRARY_PATH'] = str(home / 'lib') + ':' + env.get('LD_LIBRARY_PATH', '')
        configs = list((home / 'lib/python3.14').glob('_sysconfigdata*.py'))
        require(len(configs) == 1, 'one real target Python sysconfig required')
        syntax = ast.parse(configs[0].read_text())
        variables = next(ast.literal_eval(node.value) for node in syntax.body
                         if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'build_time_vars' for t in node.targets))
        require(variables['VERSION'] == '3.14' and variables['LDLIBRARY'] == 'libpython3.14.so'
                and variables['Py_ENABLE_SHARED'] == 1 and variables.get('Py_GIL_DISABLED', 0) == 0
                and variables['SIZEOF_VOID_P'] == 8 and variables['MULTIARCH'] == arch + '-linux-gnu', 'real pinned standard Python configuration differs')
        library = home / 'lib' / variables['LDLIBRARY']
        require(library.resolve().is_relative_to(home.resolve()), 'real libpython leaves runtime')
        with library.open('rb') as file:
            header = file.read(20)
        machine = b'\xb7\x00' if arch == 'aarch64' else b'\x3e\x00'
        require(header[:6] == b'\x7fELF\x02\x01' and header[18:20] == machine, 'actual libpython ELF target differs')
        config_file = work / 'pyo3-target.txt'
        config_file.write_text('implementation=CPython\nversion=' + variables['VERSION']
                               + '\nshared=true\nabi3=false\nlib_name=' + variables['LDLIBRARY'][3:-3]
                               + '\nlib_dir=' + str(home / 'lib') + '\npointer_width=64\nbuild_flags=\n')
        env['PYO3_CONFIG_FILE'] = str(config_file)
        report['pyo3_config'] = {'path': str(config_file), 'sha256': sha(config_file),
                                 'sysconfig_sha256': sha(configs[0]), 'actual_lib_dir': str(home / 'lib'),
                                 'original_build_lib_dir': variables.get('LIBDIR'), 'target': target}
        save()
        strip = Path('/usr/bin/strip')
        if arch == 'aarch64':
            env['PYO3_PYTHON'] = str(home / 'bin/python3')
            call([home / 'bin/python3', '-I', '-B', '-c', 'import sys,pyarrow; print(sys.version); print(pyarrow.__version__)'], 'python-runtime')
        else:
            loaded = importlib.util.spec_from_file_location('cross', '/tmp/cross-tools-linux-smoke.py')
            cross = importlib.util.module_from_spec(loaded)
            loaded.loader.exec_module(cross)
            cb = json.loads(Path('/inputs/cross/bundle.json').read_text())
            require(sha(Path('/inputs/cross/cross-tools.tar.gz')) == cb['archive_sha256'], 'GNU cross archive differs')
            tree = cross.manifest(Path('/inputs/cross/manifest.json'), cb['manifest_sha256'])
            tools = work / 'cross'
            cross.extract(Path('/inputs/cross/cross-tools.tar.gz'), tree, tools, cb)
            gb = json.loads(Path('/tmp/kd3-gnu-bundle.json').read_text())
            ga = Path('/inputs/kd3-gnu') / Path(gb['archive']).name
            gm = Path('/inputs/kd3-gnu') / Path(gb['manifest']).name
            require(sha(ga) == gb['archive_sha256'] and sha(gm) == gb['manifest_sha256'], 'GNU Rust target archive differs')
            version = call(['rustup', 'run', '1.98.1', 'rustc', '--version'], 'rust-version')
            require(version.strip().startswith('rustc ' + gb['rust_std_version']), 'actual Rust compiler/target commit differs')
            root = Path(subprocess.check_output(['rustup', 'run', '1.98.1', 'rustc', '--print', 'sysroot'], text=True).strip()) / 'lib/rustlib'
            expected = {m['path']: m for m in json.loads(gm.read_text())}
            with tarfile.open(ga) as archive:
                seen=set()
                members = archive.getmembers()
                require(len(members) == len(expected), 'GNU target member count differs')
                for m in members:
                    require(m.isfile() and m.name in expected and m.name not in seen and m.name.startswith('x86_64-unknown-linux-gnu/') and '..' not in Path(m.name).parts, 'GNU target member escape')
                    seen.add(m.name)
                    require(m.size == expected[m.name]['bytes'], 'GNU target member size differs')
                    require(hashlib.file_digest(archive.extractfile(m), 'sha256').hexdigest() == expected[m.name]['sha256'], 'GNU target member SHA differs')
                require(seen == set(expected), 'GNU target manifest coverage differs')
                extract_fresh(archive, root, members, 'x86_64-unknown-linux-gnu')
            env['LD_LIBRARY_PATH'] = str(tools / 'usr/lib/aarch64-linux-gnu') + ':' + env['LD_LIBRARY_PATH']
            common = cross.compiler_arguments(tools)
            gnu = tools / 'usr/x86_64-linux-gnu'
            libgcc = tools / 'usr/lib/gcc-cross/x86_64-linux-gnu/12'
            flags = common + ['-B' + str(gnu / 'lib') + '/', '-nostdinc', '-isystem', libgcc / 'include', '-isystem', gnu / 'include', '-L' + str(gnu / 'lib')]
            linker = work / 'gnu-cc'
            linker.write_text('#!/bin/sh\nexec ' + shlex.join([str(x) for x in flags]) + ' "$@"\n')
            linker.chmod(0o700)
            env.update(CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=str(linker), CC_x86_64_unknown_linux_gnu=str(linker),
                       AR_x86_64_unknown_linux_gnu=str(tools / 'usr/bin/x86_64-linux-gnu-ar'))
            # Reuse the already-proved sole C++ object/undefined-symbol checker;
            # only the real target/sysroot switches to the GNU headers here.
            loader = importlib.util.spec_from_file_location('personality_producer', '/tmp/personality-producer.py')
            previous = importlib.util.module_from_spec(loader)
            loader.loader.exec_module(previous)
            clang = Path('/opt/moruna-proof-tools/usr/bin/clang++-14')
            resource = Path('/opt/moruna-proof-tools/usr/lib/llvm-14/lib/clang/14.0.6/include')
            target_args = ['--target=x86_64-linux-gnu', '-nostdinc++', '-nostdinc', '-isystem', str(resource), '-isystem', str(gnu / 'include'), '-isystem', str(libgcc / 'include')]
            allowed = next(ast.literal_eval(node.value) for node in ast.walk(ast.parse(Path('/tmp/personality-producer.py').read_text()))
                           if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'allowed' for t in node.targets)
                           and isinstance(node.value, ast.List))
            definitions = call([tools / 'usr/bin/x86_64-linux-gnu-nm', '-g', '--defined-only', '--format=posix', gnu / 'lib/libc.a'], 'gnu-libc-defined')
            libc = gnu / 'lib/libc.a'
            require(sha(libc) == '8a78b8c0ecfc87dfaa667d755eb04769c0783671b890f80ed6dfb6c8cc2d211b', 'actual authenticated GNU libc differs')
            bcmp = [line for line in definitions.splitlines() if len(line.split()) >= 2 and line.split()[0] == 'bcmp' and line.split()[1] in ('T', 'i', 'I')]
            require(bcmp, 'actual GNU libc must define bcmp (function or GNU IFUNC)')
            report['gnu_libc'] = {'sha256': sha(libc), 'defined_bcmp': bcmp}
            save()
            allowed.append('bcmp')
            config = {'compiler': [str(clang), *target_args], 'compiler_sha256': sha(clang), 'nm': str(tools / 'usr/bin/x86_64-linux-gnu-nm'), 'allowed': allowed, 'out': str(out)}
            cxx = work / 'gnu-cxx'
            cxx.write_text(previous.CXX_WRAPPER.replace('__CONFIG__', repr(config)).replace('actual musl runtime', 'actual GNU runtime'))
            cxx.chmod(0o700)
            env.update(CXX_x86_64_unknown_linux_gnu=str(cxx), CXXSTDLIB_x86_64_unknown_linux_gnu='',
                       BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu=shlex.join(target_args))
            strip = tools / 'usr/bin/x86_64-linux-gnu-strip'
        base = ['rustup', 'run', '1.98.1', 'cargo', 'build', '--locked', '--offline', '--target', target,
                '--config', 'profile.dev.debug=0', '--config', 'profile.dev.incremental=false', '-j1',
                '-p', 'moruna-py', '--features', 'python,peql', '--lib', '--bin', 'moruna-kd3-fixture']
        call(base, 'cargo-build')
        for relative, name, executable in [('debug/moruna-kd3-fixture', 'moruna-kd3-fixture', True), ('debug/lib_core.so', '_core.so', False)]:
            source = Path('/proof/target') / target / relative
            require(source.is_file() and not source.is_symlink() and 0 < source.stat().st_size <= 1024**3, 'fixture raw artifact missing/over1GiB')
            raw_bytes, raw_hash = source.stat().st_size, sha(source)
            dest = out / name
            shutil.copyfile(source, dest)
            dest.chmod(0o700 if executable else 0o600)
            call([strip, '--strip-unneeded', dest], 'strip-' + name.replace('.', '-'))
            with dest.open('rb') as f:
                header = f.read(20)
            machine = b'\xb7\x00' if arch == 'aarch64' else b'\x3e\x00'
            require(header[:6] == b'\x7fELF\x02\x01' and header[18:20] == machine and 0 < dest.stat().st_size <= 256 * 1024**2, 'actual target ELF/256MiB bound differs')
            report['files'].append({'name': name, 'bytes': dest.stat().st_size, 'sha256': sha(dest), 'original_path': str(source),
                                    'original_bytes': raw_bytes, 'original_sha256': raw_hash, 'strip_tool_sha256': sha(strip),
                                    'strip_version': subprocess.check_output([str(strip), '--version'], text=True).splitlines()[0]})
            save()
        if arch == 'aarch64':
            shutil.copytree('/proof/source/python/moruna', site / 'moruna')
            shutil.copyfile(out / '_core.so', site / 'moruna/_core.so')
            call([home / 'bin/python3', '-I', '-B', '-c', 'import moruna,pyarrow; print(moruna.__version__); print(pyarrow.__version__)'], 'actual-module-import')
        report['python'] = {'archive_sha256': python['sha256'], 'pyarrow_sha256': arrow['sha256'], 'version': '3.14', 'implementation': 'CPython', 'gil_disabled': False}
        report['passed'] = True
    finally:
        save()


if __name__ == '__main__':
    main()
