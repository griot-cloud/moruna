#!/usr/bin/env python3
"""Bounded offline KE2 Linux stages; prepared-only unless --run is supplied."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import sys
import tarfile

from attachments import create_cache, flags, private_directory, validate_cache, validate_inspect
from run import ROOT, LABEL, Run, require, sha


GUEST = r'''set -euo pipefail
token="$1"; phase="$2"; archive_sha="$3"
printf 'MORUNA_KE2_BEGIN %s %s\n' "$token" "$phase"
printf '%s  /tmp/moruna-source.tar\n' "$archive_sha" | sha256sum -c -
mkdir -p /proof/source /proof/cargo
tar -xf /tmp/moruna-source.tar -C /proof/source
cd /proof/source
test -f kernel/Cargo.lock
test -f .cargo/config.toml
test -d vendor
export CARGO_HOME=/proof/cargo CARGO_TARGET_DIR=/proof/target
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2
rustup toolchain list | grep -q '^1\.98\.1-aarch64-unknown-linux-gnu'
rustup run 1.98.1 rustc --version
run_cargo() {
  rustup run 1.98.1 cargo test --no-fail-fast --locked --offline --manifest-path kernel/Cargo.toml \
    --target aarch64-unknown-linux-gnu --config profile.dev.debug=0 \
    --config profile.dev.incremental=false --config profile.test.debug=0 \
    --config profile.test.incremental=false -j2 "$@"
}
case "$phase" in
  image-tools)
    export CARGO_BUILD_JOBS=1 RUSTFLAGS="--cfg polyval_force_soft"
    unset CARGO_ENCODED_RUSTFLAGS
    mkdir -p /proof/image-tools
    retain_image_tool() {
      python3 - "$1" "$2" <<'PY_IMAGE_TOOL'
import hashlib,json,pathlib,shutil,stat,subprocess,sys
source=pathlib.Path(sys.argv[1]);name=sys.argv[2];out=pathlib.Path('/proof/image-tools');info=source.lstat()
if not stat.S_ISREG(info.st_mode) or not info.st_mode&0o111 or not 0<info.st_size<=512*1024**2:raise SystemExit('Required raw image tool type/512MiB bound differs: '+name)
with source.open('rb') as f:header=f.read(20)
if header[:6]!=b'\x7fELF\x02\x01' or header[18:20]!=b'\xb7\x00':raise SystemExit('Actual ARM64 Linux ELF required: '+name)
def sha(path):
    hash=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024**2),b''):hash.update(chunk)
    return hash.hexdigest()
manifest=out/'artifacts.json'
record=json.loads(manifest.read_text()) if manifest.exists() else {'profile':'dev-debug0','cache_profile':'test-debug0','target':'aarch64-unknown-linux-gnu','toolchain':'1.98.1','jobs':1,'scope':'host image parser/writer build only; no optimization/performance/boot claim','files':[]}
if name not in ('moruna-image','moruna-node-image') or any(item['name']==name for item in record['files']):raise SystemExit('Unexpected/duplicate image helper')
if sum(item['original_bytes'] for item in record['files'])+info.st_size>1024**3:raise SystemExit('Raw image helper aggregate exceeds1GiB')
original_sha=sha(source);destination=out/name
if destination.exists():raise SystemExit('Image helper destination already exists')
strip=pathlib.Path('/usr/bin/strip')
if not strip.is_file():raise SystemExit('Actual Linux-native strip required')
strip_version=subprocess.check_output([str(strip),'--version'],text=True).splitlines()[0];strip_sha=sha(strip)
shutil.copy2(source,destination)
subprocess.run([str(strip),'--strip-unneeded',str(destination)],check=True)
shipped=destination.stat()
with destination.open('rb') as f:header=f.read(20)
if not 0<shipped.st_size<=128*1024**2 or not shipped.st_mode&0o111 or header[:6]!=b'\x7fELF\x02\x01' or header[18:20]!=b'\xb7\x00':raise SystemExit('Shipped image helper ELF/128MiB bound differs')
if sum(item['bytes'] for item in record['files'])+shipped.st_size>256*1024**2:raise SystemExit('Shipped image helper aggregate exceeds256MiB')
if source.stat().st_size!=info.st_size or sha(source)!=original_sha:raise SystemExit('Raw cache tool changed while stripping retained copy')
record['files'].append({'name':name,'bytes':shipped.st_size,'sha256':sha(destination),'original_path':str(source),'original_bytes':info.st_size,'original_sha256':original_sha,'strip_tool_sha256':strip_sha,'strip_version':strip_version})
manifest.write_text(json.dumps(record,indent=2)+'\n')
print('MORUNA_IMAGE_TOOL_RETAINED',name,info.st_size,original_sha,shipped.st_size,record['files'][-1]['sha256'])
PY_IMAGE_TOOL
    }
    for tool in moruna-image moruna-node-image; do
      rustup run 1.98.1 cargo build --locked --offline --manifest-path kernel/Cargo.toml \
        --target aarch64-unknown-linux-gnu --config profile.dev.debug=0 \
        --config profile.dev.incremental=false --config profile.test.debug=0 \
        --config profile.test.incremental=false -j1 -p "$tool" --bin "$tool"
      retain_image_tool "/proof/target/aarch64-unknown-linux-gnu/debug/$tool" "$tool"
    done
    ;;
  integrated)
    run_cargo -p moruna-svc -p netstack-protocol -p moruna-netstack -p block-protocol \
      -p moruna-storage-service -p moruna-node -p node-protocol -p moruna-node-image -p moruna-image \
      -p moruna-imagefs -p imagefs-protocol -p moruna-linux -p linux-protocol -- --test-threads=2
    ;;
  memory)
    run_cargo -p moruna-svc -p netstack-protocol -- --test-threads=2
    run_cargo -p moruna-netstack --test tls -- --test-threads=2
    ;;
  all)
    run_cargo -p netstack-protocol -- --test-threads=2
    run_cargo -p moruna-netstack --lib --test loopback_dns --test tls --test suites --test robust -- --test-threads=2
    ;;
  compile) run_cargo -p netstack-protocol -p moruna-netstack --no-run ;;
  tests)
    run_cargo -p netstack-protocol -- --test-threads=2
    run_cargo -p moruna-netstack --lib --test loopback_dns --test tls --test suites -- --test-threads=2
    ;;
  robust) run_cargo -p moruna-netstack --test robust -- --test-threads=2 ;;
  tls) run_cargo -p moruna-netstack --test tls -- --test-threads=2 ;;
  *) exit 2 ;;
esac
printf 'MORUNA_KE2_DONE %s %s\n' "$token" "$phase"
'''


def private_file(raw):
    path = Path(raw)
    # macOS exposes its system-owned /private/var tree through /var. Normalize
    # that one system alias, then reject links in every private path component.
    if str(path).startswith('/var/folders/') and str(ROOT).startswith('/private/var/folders/'):
        path = Path('/private' + str(path))
    require(path.is_absolute() and path.is_relative_to(ROOT), 'input outside private Linux proof root')
    private_directory(path.parent)
    info = path.lstat()
    require(path.is_file() and not path.is_symlink() and info.st_uid == os.getuid()
            and info.st_nlink == 1, 'input must be an owned regular private file')
    return path


def validate_inputs(inputs, archive, *, lock_path='kernel/Cargo.lock',
                    lock_key='kernel_cargo_lock_sha256', max_files=20000, max_bytes=400 * 1024 * 1024):
    require(sha(archive) == inputs['archive_sha256'], 'archive does not match provenance')
    census_path = private_file(inputs['inputs_census_path'])
    require(sha(census_path) == inputs['inputs_census_sha256'], 'input census changed')
    census = json.loads(census_path.read_text())
    require(len(census) == inputs['inputs_files'] and len(census) <= max_files, 'input file count differs')
    expected = {item['path']: item for item in census}
    require(len(expected) == len(census), 'duplicate input census path')
    require(expected[lock_path]['sha256'] == inputs[lock_key], 'source lock binding differs')
    found = set()
    total = 0
    with tarfile.open(archive) as source:
        for member in source:
            path = Path(member.name)
            require(not path.is_absolute() and '..' not in path.parts, 'archive path escape')
            if member.isdir():
                continue
            require(member.isfile() and member.name in expected and member.name not in found,
                    'unexpected/duplicate/nonregular archive input')
            item = expected[member.name]
            require(item['type'] == 'file' and member.size == item['bytes']
                    and member.mode == item['mode'], 'input metadata differs')
            total += member.size
            require(total <= max_bytes, 'input archive exceeds approved size bound')
            digest = hashlib.sha256()
            file = source.extractfile(member)
            require(file is not None, 'input archive file missing')
            with file:
                for chunk in iter(lambda: file.read(1024 * 1024), b''):
                    digest.update(chunk)
            require(digest.hexdigest() == item['sha256'], 'archive input checksum differs: ' + member.name)
            found.add(member.name)
    require(found == expected.keys() and total == inputs['inputs_bytes'], 'archive census incomplete')


class Ke2Run(Run):
    guest_script = GUEST
    marker = 'MORUNA_KE2_DONE'
    lock_key = 'kernel_cargo_lock_sha256'
    lock_label = 'kernel_lock_sha256'
    runner_files = ('ke2-linux.py', 'run.py', 'attachments.py', 'native-toolchain-inventory.py')

    def __init__(self, options, inputs, archive, cache):
        super().__init__(options)
        self.inputs = inputs
        self.archive_path = archive
        self.cache = cache
        self.memory = 4
        self.attachments = [cache]
        if options.phase=='image-tools':
            require(not cache.get('requires_local_clean'), 'image tools require fresh or exact same-source cache; no retargeting')
            require(cache['source_lock_sha256']==inputs['kernel_cargo_lock_sha256'] and cache['inputs_census_sha256']==inputs['inputs_census_sha256'] and cache['toolchain']=='1.98.1' and cache['target']=='aarch64-unknown-linux-gnu' and cache['profile']=='test-debug0', 'image tools cache must match exact ARM GNU source/profile')
            self.measurement_scope='ARM64 Linux image parser/writer dev-debug0 build and custody only; no optimization/performance/boot claim'

    def call(self, arguments, *args, **kwargs):
        result=super().call(arguments, *args, **kwargs)
        if self.options.phase=='image-tools' and len(arguments)>4 and arguments[:4]==['exec',self.name,'/bin/bash','-c'] and arguments[4]==self.guest_script:
            require(self.owned() is not None, 'owned image-tools VM missing before collection')
            super().call(['copy',self.name+':/proof/image-tools',str(self.evidence/'image-tools')],seconds=45,check=True)
            if result[0]==0:
                folder=self.evidence/'image-tools';record=json.loads((folder/'artifacts.json').read_text())
                require(record.get('profile')=='dev-debug0' and record.get('cache_profile')=='test-debug0' and record.get('target')=='aarch64-unknown-linux-gnu' and record.get('toolchain')=='1.98.1' and record.get('jobs')==1, 'image helper actual profile differs')
                require({item['name'] for item in record['files']}=={'moruna-image','moruna-node-image'} and len(record['files'])==2, 'required actual image tools missing')
                total=0;raw_total=0
                for item in record['files']:
                    path=folder/item['name'];info=path.lstat();total+=info.st_size;raw_total+=item['original_bytes']
                    require(stat.S_ISREG(info.st_mode) and info.st_mode&0o111 and 0<info.st_size<=128*1024**2 and info.st_size==item['bytes'], 'retained image tool type/size differs')
                    with path.open('rb') as file:header=file.read(20)
                    require(header[:6]==bytes([127,69,76,70,2,1]) and header[18:20]==bytes([183,0]) and sha(path)==item['sha256'], 'retained image tool ELF/hash differs')
                    require(0<item['original_bytes']<=512*1024**2 and len(item['original_sha256'])==64 and len(item['strip_tool_sha256'])==64 and item['strip_version'], 'image helper raw/strip provenance missing')
                require(total<=256*1024**2 and raw_total<=1024**3, 'image helper aggregate exceeds declared bound')
        return result

    def owned(self, cleanup=False):
        record = super().owned(cleanup=cleanup)
        if record is not None:
            validate_cache(self.cache, self.cache['owner_nonce'])
            validate_inspect(record['configuration'], self.attachments)
            config = record['configuration']
            require(config['image']['descriptor']['digest'] == self.pins['index'], 'owned VM base changed')
            require(config['platform'] == {'os': 'linux', 'architecture': 'arm64'}, 'owned VM platform changed')
            require(config['resources']['memoryInBytes'] == self.memory * 1024**3, 'owned VM memory changed')
        return record

    def execute(self):
        self.evidence.mkdir(mode=0o700)
        summary = {
            'name': self.name, 'nonce': self.token, 'mode': self.options.mode,
            'phase': self.options.phase, 'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'cpus': 2, 'memory_gib': self.memory, 'pins': self.pins,
            'input_manifest_sha256': sha(Path(self.options.inputs)),
            'source_archive_sha256': self.inputs['archive_sha256'],
            self.lock_label: self.inputs[self.lock_key],
            'source_head': self.inputs['source_head'],
            'source_dirty_diff_sha256': self.inputs['source_dirty_diff_sha256'],
            'source_census_sha256': self.inputs['source_census_sha256'],
            'inputs_census_sha256': self.inputs['inputs_census_sha256'],
            'cache': self.cache, 'cleanup': False,
            'readonly_tools': getattr(self, 'tool_bundle', None),
            'scripts': {name: sha(ROOT / name) for name in self.runner_files},
            'measurement_scope': getattr(self, 'measurement_scope', 'native ARM Linux compile/tests only; no kernel boot or workload timing claim'),
        }
        failure = None
        try:
            require(sha(self.archive_path) == self.inputs['archive_sha256'], 'source archive changed')
            validate_cache(self.cache, self.cache['owner_nonce'])
            self.cached_prerequisites()
            self.call(['create', '--name', self.name, '--label', LABEL + '=' + self.token,
                       '--cpus', '2', '--memory', f'{self.memory}G', '--platform', 'linux/arm64',
                       '--init-image', self.pins['init'], '--kernel', self.pins['kernel_path'],
                       '--no-dns', *flags(self.attachments), '--entrypoint', '/bin/bash',
                       self.pins['image'], *self.arguments], seconds=45)
            require(self.owned() is not None, 'new owned VM missing')
            self.call(['start', self.name])
            require(self.owned() is not None, 'owned VM missing before source transfer')
            self.call(['copy', str(self.archive_path), self.name + ':/tmp/moruna-source.tar'], seconds=90)
            require(self.owned() is not None, 'owned VM missing before proof')
            if self.options.phase == 'integrated':
                self.call(['copy', str(ROOT / 'native-toolchain-inventory.py'), self.name + ':/tmp/native-toolchain-inventory.py'])
                self.call(['exec', self.name, '/bin/mkdir', '-p', '/proof/source'])
                self.call(['exec', self.name, '/bin/tar', '-xf', '/tmp/moruna-source.tar', '-C', '/proof/source'], seconds=60)
                self.call(['exec', self.name, 'python3', '/tmp/native-toolchain-inventory.py'], seconds=30)
                self.call(['copy', self.name + ':/proof/native-inventory', str(self.evidence / 'native-inventory')], seconds=30)
            code, output = self.call(['exec', self.name, '/bin/bash', '-c', self.guest_script, '--', self.token,
                                      self.options.phase, self.inputs['archive_sha256']],
                                     seconds=self.options.seconds, check=False)
            summary['guest_exit'] = code
            require(code == 0 and f'{self.marker} {self.token} {self.options.phase}' in output,
                    self.options.mode + ' stage failed or lacked its completion marker')
            summary['stage_passed'] = True
        except BaseException as error:
            failure = error
            summary['error'] = f'{type(error).__name__}: {error}'
        finally:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            try:
                self.cleanup()
                summary['cleanup'] = True
            except BaseException as error:
                summary['cleanup_error'] = f'{type(error).__name__}: {error}'
                failure = error
            summary['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            (self.evidence / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
            if summary['cleanup'] and self.fingerprint == (self.lock.stat().st_dev, self.lock.stat().st_ino) \
                    and (self.lock / 'owner').read_text().strip() == self.owner:
                (self.lock / 'owner').unlink()
                self.lock.rmdir()
            print(json.dumps(summary), flush=True)
        return 1 if failure else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--phase', choices=('integrated', 'memory', 'all', 'compile', 'tests', 'robust', 'tls', 'image-tools'), required=True)
    parser.add_argument('--inputs', required=True)
    parser.add_argument('--archive', required=True)
    parser.add_argument('--cache-journal')
    parser.add_argument('--seconds', type=int, default=120)
    options = parser.parse_args()
    options.mode = 'ke2-linux'
    require(30 <= options.seconds <= 600, 'KE2 stage must be bounded to 30..600 seconds')
    inputs = json.loads(private_file(options.inputs).read_text())
    archive = private_file(options.archive)
    validate_inputs(inputs, archive)
    if not options.run:
        print(json.dumps({'prepared_only': True, 'phase': options.phase, 'source_archive_sha256': inputs['archive_sha256']}))
        return 0
    os.umask(0o077)
    if options.cache_journal:
        cache = json.loads(private_file(options.cache_journal).read_text())
        validate_cache(cache, cache['owner_nonce'])
        require(cache['source_lock_sha256'] == inputs['kernel_cargo_lock_sha256']
                and cache.get('inputs_census_sha256') == inputs['inputs_census_sha256']
                and cache['toolchain'] == '1.98.1' and cache['target'] == 'aarch64-unknown-linux-gnu'
                and cache['profile'] == 'test-debug0', 'cache identity differs from exact proof inputs')
    else:
        import secrets
        cache = create_cache(secrets.token_hex(16), inputs['kernel_cargo_lock_sha256'], '1.98.1',
                             'aarch64-unknown-linux-gnu', 'test-debug0',
                             inputs_sha256=inputs['inputs_census_sha256'])
    run = Ke2Run(options, inputs, archive, cache)
    # Validate tar members before any resource acquisition/VM creation.
    run.archive(str(archive))
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
