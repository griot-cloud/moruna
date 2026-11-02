#!/usr/bin/env python3
"""Prepared Apple Container proof runner. --run is required; never installs or builds images."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys
import tarfile
import time

ROOT = Path(__file__).resolve().parent
CLI = '/opt/homebrew/bin/container'
LABEL = 'io.moruna.proof.owner'


def require(ok, text):
    if not ok:
        raise RuntimeError(text)


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


class Run:
    def __init__(self, options):
        self.options = options
        self.pins = json.loads((ROOT / 'pins.json').read_text())
        self.token = secrets.token_hex(16)
        self.name = f'moruna-proof-{os.getuid()}-{self.token[:12]}'
        self.owner = f'codex moruna linux {self.token} {datetime.datetime.now(datetime.timezone.utc).isoformat()}'
        self.lock = Path.home() / '.vm-lock'
        self.fingerprint = None
        self.deadline = time.monotonic() + options.seconds
        self.evidence = ROOT / self.name
        self.number = 0
        self.guest = (ROOT / ('preflight.sh' if options.mode == 'preflight' else 'proof.sh')).read_text()
        self.arguments = ['-c', self.guest, '--', self.token]
        if options.mode != 'preflight':
            # Proof init waits for a separately copied source archive and reviewed command.
            self.guest = 'printf "MORUNA_OWNED_INIT %s\\n" "$1"; exec /bin/sleep 7200'
            self.arguments = ['-c', self.guest, '--', self.token]
        self.memory = 2 if options.mode == 'preflight' else 6
        self.env = {'HOME': str(Path.home()), 'PATH': '/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin', 'LANG': 'C', 'LC_ALL': 'C'}

    def acquire(self):
        turn = Path.home() / '.vm-lock-next'
        reservation = turn.read_text().strip() if turn.exists() else ''
        require(reservation in ('', 'moruna'), 'DEFER: reserved turn ' + reservation)
        require(shutil.disk_usage(ROOT).free >= 8 * 1024**3, 'DEFER: shared 8 GiB admission floor')
        try:
            self.lock.mkdir(mode=0o700)
        except FileExistsError:
            raise RuntimeError('DEFER: shared lock occupied; never steal it')
        (self.lock / 'owner').write_text(self.owner + '\n')
        self.fingerprint = (self.lock.stat().st_dev, self.lock.stat().st_ino)

    def guard(self):
        require(self.fingerprint == (self.lock.stat().st_dev, self.lock.stat().st_ino), 'shared lock identity changed')
        require(not self.lock.is_symlink() and (self.lock / 'owner').read_text().strip() == self.owner, 'shared lock owner changed')
        require(time.monotonic() < self.deadline, 'global stage deadline exceeded')
        require(shutil.disk_usage(ROOT).free >= 6 * 1024**3, 'free space below shared 6 GiB hard floor')

    def call(self, arguments, seconds=30, check=True, cleanup=False):
        if not cleanup:
            self.guard()
        self.number += 1
        path = self.evidence / f'{self.number:03d}-{arguments[0]}.log'
        process = None
        try:
            with path.open('xb') as output:
                process = subprocess.Popen([CLI, *arguments], stdout=output, stderr=subprocess.STDOUT,
                                           stdin=subprocess.DEVNULL, env=self.env, start_new_session=True)
                end = time.monotonic() + seconds
                while process.poll() is None:
                    if not cleanup:
                        self.guard()
                    require(time.monotonic() < end, 'command timeout: ' + arguments[0])
                    require(path.stat().st_size <= 16 * 1024 * 1024, 'command output exceeded 16 MiB')
                    time.sleep(0.2)
            text = path.read_text(errors='replace')
            require(not check or process.returncode == 0, f'{arguments[0]} exited {process.returncode}; see {path}')
            return process.returncode, text
        finally:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)

    def json_call(self, arguments, **kwargs):
        code, text = self.call(arguments, **kwargs)
        return code, json.loads(text) if code == 0 else None

    def owned(self, cleanup=False):
        code, records = self.json_call(['inspect', self.name], check=False, cleanup=cleanup)
        if code:
            _, all_records = self.json_call(['list', '--all', '--format', 'json'], cleanup=cleanup)
            require(not any(item.get('id') == self.name for item in all_records), 'inspect failed for an existing VM')
            return None
        require(isinstance(records, list) and len(records) == 1, 'ambiguous VM inspect')
        record = records[0]
        config = record.get('configuration', {})
        require(record.get('id') == self.name and config.get('id') == self.name, 'VM identity differs')
        require(config.get('labels', {}).get(LABEL) == self.token, 'VM nonce label differs')
        require(config.get('initProcess', {}).get('executable') == '/bin/bash'
                and config['initProcess'].get('arguments') == self.arguments, 'VM initial process differs')
        return record

    def cleanup(self):
        record = self.owned(cleanup=True)
        if record is None:
            return
        # Revalidate immediately before each destructive operation; no global stop/delete.
        self.owned(cleanup=True)
        self.call(['stop', '--time', '3', self.name], seconds=15, check=False, cleanup=True)
        require(self.owned(cleanup=True) is not None, 'owned VM disappeared before delete')
        self.call(['delete', '--force', self.name], seconds=25, cleanup=True)
        require(self.owned(cleanup=True) is None, 'owned VM remains after deletion')

    def cached_prerequisites(self):
        _, status = self.json_call(['system', 'status', '--format', 'json'])
        require(status['status'] == 'running' and status['client']['version'] == self.pins['cli'], 'reviewed running CLI service required')
        _, images = self.json_call(['image', 'inspect', self.pins['image']])
        require(len(images) == 1 and images[0]['configuration']['descriptor']['digest'] == self.pins['index'], 'cached base index changed; no pull attempted')
        require(any(v['digest'] == self.pins['variant'] and v['platform']['architecture'] == 'arm64' for v in images[0]['variants']), 'cached arm64 variant differs')
        _, properties = self.json_call(['system', 'property', 'list', '--format', 'json'])
        require(properties['vminit']['image'] == self.pins['init'], 'default init changed')
        require(properties['kernel'] == self.pins['kernel_property'], 'kernel properties changed')
        kernel = Path(self.pins['kernel_path'])
        require(kernel.is_file() and not kernel.is_symlink() and sha(kernel) == self.pins['kernel_sha256'], 'cached kernel bytes differ')
        # CLI 1.6.1 hides configured infrastructure images in inspect output but must
        # successfully resolve the cached reference. No image pull or service start.
        self.json_call(['image', 'inspect', self.pins['init']])
        require(self.owned() is None, 'new nonce VM name already exists')

    def archive(self, raw):
        path = Path(raw).resolve(strict=True)
        require(path.is_relative_to(ROOT) and path.is_file(), 'archive must be a regular private Linux-proof input')
        with tarfile.open(path) as archive:
            for member in archive:
                parts = Path(member.name)
                require(not parts.is_absolute() and '..' not in parts.parts, 'archive path escape')
                require(member.isfile() or member.isdir(), 'archive links/devices/FIFOs are not accepted')
        return path

    def execute(self):
        self.evidence.mkdir(mode=0o700)
        summary = {'name': self.name, 'nonce': self.token, 'mode': self.options.mode, 'cpus': 2,
                   'memory_gib': self.memory, 'pins': self.pins, 'cleanup': False,
                   'scripts': {name: sha(ROOT / name) for name in ('run.py', 'preflight.sh', 'proof.sh')}}
        failure = None
        try:
            self.cached_prerequisites()
            args = ['create', '--name', self.name, '--label', LABEL + '=' + self.token,
                    '--cpus', '2', '--memory', f'{self.memory}G', '--platform', 'linux/arm64',
                    '--init-image', self.pins['init'], '--kernel', self.pins['kernel_path'],
                    '--no-dns', '--entrypoint', '/bin/bash', self.pins['image'], *self.arguments]
            self.call(args, seconds=45)
            config = self.owned()['configuration']
            require(config['image']['descriptor']['digest'] == self.pins['index'], 'created VM image differs')
            require(config['platform']['architecture'] == 'arm64', 'created VM is not arm64')
            require(config['resources']['cpus'] == 2 and config['resources']['memoryInBytes'] == self.memory * 1024**3, 'VM resources differ')
            require(not any(config.get(key) for key in ('mounts', 'publishedPorts', 'publishedSockets', 'ssh', 'virtualization', 'rosetta', 'capAdd')), 'VM gained forbidden attachments/capabilities')
            if self.options.mode == 'preflight':
                code, text = self.call(['start', '--attach', self.name], seconds=self.options.seconds, check=False)
                summary['guest_exit'] = code
                require(code == 0 and f'MORUNA_PREFLIGHT_NONCE {self.token}' in text
                        and f'MORUNA_PREFLIGHT_DONE {self.token}' in text, 'preflight did not finish successfully')
            else:
                source = self.archive(self.options.source_archive)
                summary['source_archive_sha256'] = sha(source)
                self.call(['start', self.name])
                require(self.owned() is not None, 'owned proof VM missing')
                self.call(['copy', str(source), self.name + ':/tmp/moruna-source.tar'], seconds=90)
                self.call(['exec', self.name, '/bin/mkdir', '-p', '/proof/source'])
                self.call(['exec', self.name, '/bin/tar', '-xf', '/tmp/moruna-source.tar', '-C', '/proof/source'], seconds=90)
                if self.options.boot_archive:
                    boot = self.archive(self.options.boot_archive)
                    summary['boot_archive_sha256'] = sha(boot)
                    self.call(['copy', str(boot), self.name + ':/tmp/moruna-boot.tar'], seconds=90)
                    self.call(['exec', self.name, '/bin/mkdir', '-p', '/proof/boot'])
                    self.call(['exec', self.name, '/bin/tar', '-xf', '/tmp/moruna-boot.tar', '-C', '/proof/boot'], seconds=90)
                script = (ROOT / 'proof.sh').read_text()
                code, text = self.call(['exec', self.name, '/bin/bash', '-c', script, '--', self.token, self.options.mode, *self.options.package], seconds=self.options.seconds, check=False)
                summary['guest_exit'] = code
                # Preserve evidence and build artifacts before teardown, even on test failure.
                if self.options.mode in ('kernel-build', 'kernel-boot'):
                    # Explicit boot artifacts only: never copy a whole Cargo target/source tree.
                    self.call(['copy', self.name + ':/proof/boot', str(self.evidence / 'boot')], seconds=90, check=code == 0)
                if self.options.mode == 'kernel-boot':
                    for cpus in (1, 2, 4):
                        self.call(['copy', self.name + f':/proof/boot-{cpus}.log', str(self.evidence / f'boot-{cpus}.log')], seconds=30, check=code == 0)
                require(code == 0 and f'MORUNA_PROOF_DONE {self.token} {self.options.mode}' in text, 'proof command/checker failed or lacked completion marker')
            summary['inventory_complete' if self.options.mode == 'preflight' else 'proof_passed'] = True
        except BaseException as error:
            failure = error
            summary['error'] = f'{type(error).__name__}: {error}'
        finally:
            # Signals during cleanup must not interrupt the teardown sequence.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            try:
                self.cleanup()
                summary['cleanup'] = True
            except BaseException as error:
                summary['cleanup_error'] = f'{type(error).__name__}: {error}'
                failure = error
            (self.evidence / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
            # A failed/unconfirmed cleanup retains the owned lock for human review.
            if summary['cleanup'] and self.fingerprint == (self.lock.stat().st_dev, self.lock.stat().st_ino) and (self.lock / 'owner').read_text().strip() == self.owner:
                (self.lock / 'owner').unlink()
                self.lock.rmdir()
            print(json.dumps(summary), flush=True)
        return 1 if failure else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--mode', choices=('preflight', 'host', 'kb5-host', 'kernel-build', 'kernel-boot'), default='preflight')
    parser.add_argument('--package', action='append', default=[], help='explicit reviewed package for host mode, repeatable')
    parser.add_argument('--source-archive')
    parser.add_argument('--boot-archive')
    parser.add_argument('--seconds', type=int, default=120)
    options = parser.parse_args()
    require(30 <= options.seconds <= 6000, 'stage deadline must be 30..6000 seconds, explicitly coordinator-approved')
    require(options.mode == 'preflight' or options.source_archive, 'proof requires explicit source archive')
    require(options.mode != 'host' or options.package, 'host mode requires explicit reviewed --package selections')
    require(options.mode != 'kernel-boot' or options.boot_archive, 'boot requires preserved built artifact archive')
    if not options.run:
        print(json.dumps({'state': 'prepared only', 'mode': options.mode, 'no_container_commands': True,
                          'build_policy': 'never starts/stops shared builder; reviewed image definitions only'}))
        return 0
    require(options.mode == 'preflight', 'full proof execution disabled pending approved immutable dependencies and validated persistent private cache attachments')
    os.umask(0o077)
    run = Run(options)
    def interrupted(signum, frame):
        raise RuntimeError(f'interrupted by signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        run.acquire()
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        return 75
    return run.execute()


if __name__ == '__main__':
    sys.exit(main())
