#!/usr/bin/env python3
"""Selected combined std runtime/storage/plane Linux gate; not bare-metal kernel acceptance."""
import argparse
import base64
import hashlib
import shutil
import stat
import time
import tomllib
from pathlib import Path
import importlib.util
import json
import os
import secrets
import signal
import sys

from attachments import create_cache, validate_cache
from run import ROOT, require

tool_spec = importlib.util.spec_from_file_location('moruna_proof_tools', ROOT / 'proof-tools.py')
tool_module = importlib.util.module_from_spec(tool_spec)
tool_spec.loader.exec_module(tool_module)

spec = importlib.util.spec_from_file_location('moruna_ke2_runner', ROOT / 'ke2-linux.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

GUEST = r'''set -euo pipefail
token="$1"; phase="$2"; archive_sha="$3"
printf 'MORUNA_COMBINED_BEGIN %s %s\n' "$token" "$phase"
printf '%s  /tmp/moruna-source.tar\n' "$archive_sha" | sha256sum -c -
mkdir -p /proof/source /proof/cargo
tar -xf /tmp/moruna-source.tar -C /proof/source
cd /proof/source
test -f Cargo.lock
test -f .cargo/config.toml
test -d vendor
export CARGO_HOME=/proof/cargo CARGO_TARGET_DIR=/proof/target
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2
if [ "$phase" = kd3-fixture-tools-arm ] || [ "$phase" = kd3-fixture-tools-gnu ] || [ "$phase" = witness-tests ] || [ "$phase" = sdk-proof-build ] || [ "$phase" = boot-host-tools ] || [ "$phase" = phase-a-tests ]; then export CARGO_BUILD_JOBS=1; fi
export LIBCLANG_PATH=/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu
export LD_LIBRARY_PATH=/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu:/opt/moruna-proof-tools/usr/lib/llvm-14/lib
python3 - <<'PY'
import ctypes, glob
libraries=[]
for name in sorted(glob.glob('/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu/libclang*.so*')):
    try:
        library=ctypes.CDLL(name)
        getattr(library, 'clang_getClangVersion')
        libraries.append(name)
    except (OSError, AttributeError):
        continue
if not libraries: raise SystemExit('Verified libclang runtime input does not load')
print('MORUNA_LIBCLANG_C_API_LOADED', libraries)
PY
rustup toolchain list | grep -q '^1\.98\.1-aarch64-unknown-linux-gnu'
rustup run 1.98.1 rustc --version
run_cargo() {
  local action="$1"; shift
  rustup run 1.98.1 cargo "$action" --locked --offline --manifest-path Cargo.toml \
    --target aarch64-unknown-linux-gnu --config profile.dev.debug=0 \
    --config profile.dev.incremental=false --config profile.test.debug=0 \
    --config profile.test.incremental=false -j"$CARGO_BUILD_JOBS" "$@"
}
# Every authored package is rebuilt when an owned cache moves to a new source
# census. Frozen archives use deterministic mtimes, so timestamp-only freshness
# must never allow a prior authored artifact to stand in for changed source.
python3 - <<'PY'
import json, pathlib, subprocess
base=['rustup','run','1.98.1','cargo']
metadata=json.loads(subprocess.check_output(base+['metadata','--locked','--offline','--format-version','1'],text=True))
source=pathlib.Path('/proof/source')
stamp=pathlib.Path('/proof/target/.moruna-authored-clean-__INPUTS_CENSUS__.json')
if stamp.exists():
    if json.loads(stamp.read_text())!={'inputs_census':'__INPUTS_CENSUS__','policy':'host+explicit-target authored clean'}:raise SystemExit('Authored cache receipt differs')
    print('MORUNA_AUTHORED_CLEAN_RECEIPT_VERIFIED','__INPUTS_CENSUS__')
    raise SystemExit(0)
packages=[]
for package in metadata['packages']:
    if package['source'] is None:
        if not pathlib.Path(package['manifest_path']).is_relative_to(source):
            raise SystemExit('Unexpected authored source outside proof input')
        packages.append(package['name'])
if not packages: raise SystemExit('No authored packages to invalidate')
if any(package['source'] is not None and package['name'] in packages for package in metadata['packages']):
    raise SystemExit('Authored package name collides with external package')
clean=base+['clean','--locked','--offline','--manifest-path','Cargo.toml']
selection=[arg for package in packages for arg in ['--package',package]]
subprocess.run(clean+selection,check=True)
subprocess.run(clean+['--target','aarch64-unknown-linux-gnu']+selection,check=True)
stamp.write_text(json.dumps({'inputs_census':'__INPUTS_CENSUS__','policy':'host+explicit-target authored clean'})+'\n')
print('MORUNA_AUTHORED_ARTIFACTS_INVALIDATED',len(packages))
PY
    retain_tool() {
      python3 - "$1" "$2" <<'PY_HOST_TOOL'
import hashlib,json,pathlib,shutil,stat,subprocess,sys
source=pathlib.Path(sys.argv[1]);name=sys.argv[2];out=pathlib.Path('/proof/witness-proof')
info=source.lstat()
if not stat.S_ISREG(info.st_mode) or not info.st_mode&0o111 or not 0<info.st_size<=512*1024**2:
    raise SystemExit('Required host tool missing, not executable, or over512MiB: '+name)
with source.open('rb') as f:header=f.read(20)
if header[:6]!=b'\x7fELF\x02\x01' or header[18:20]!=b'\xb7\x00':raise SystemExit('Required real ARM64 Linux ELF differs: '+name)
manifest=out/'boot-host-tools.json'
record=json.loads(manifest.read_text()) if manifest.exists() else {'profile':'dev-debug0','target':'aarch64-unknown-linux-gnu','toolchain':'1.98.1','jobs':1,'scope':'host helper build only; no optimization/performance/boot/execution claim','files':[]}
if any(item['name']==name for item in record['files']):raise SystemExit('Duplicate retained host tool: '+name)
if sum(item['original_bytes'] for item in record['files'])+info.st_size>2*1024**3:raise SystemExit('Raw host helper aggregate exceeds2GiB')
destination=out/name
if destination.exists():raise SystemExit('Retained destination already exists: '+name)
def sha(path):
    hash=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024**2),b''):hash.update(chunk)
    return hash.hexdigest()
original_sha=sha(source)
strip=pathlib.Path('/usr/bin/strip')
if not strip.is_file():raise SystemExit('Actual Linux-native /usr/bin/strip required')
strip_sha=sha(strip)
strip_version=subprocess.check_output([str(strip),'--version'],text=True).splitlines()[0]
shutil.copy2(source,destination)
subprocess.run([str(strip),'--strip-unneeded',str(destination)],check=True)
shipped=destination.stat()
with destination.open('rb') as f:header=f.read(20)
if not 0<shipped.st_size<=128*1024**2 or not shipped.st_mode&0o111 or header[:6]!=b'\x7fELF\x02\x01' or header[18:20]!=b'\xb7\x00':raise SystemExit('Stripped ARM64 helper exceeds128MiB or ELF differs: '+name)
if sum(item['bytes'] for item in record['files'])+shipped.st_size>1024*1024**2:raise SystemExit('Shipped helper aggregate exceeds1GiB')
if source.stat().st_size!=info.st_size or sha(source)!=original_sha:raise SystemExit('Raw cache binary changed during retained-copy strip')
shipped_sha=sha(destination)
record['files'].append({'name':name,'bytes':shipped.st_size,'sha256':shipped_sha,'original_bytes':info.st_size,'original_sha256':original_sha,'original_path':str(source),'strip_tool_sha256':strip_sha,'strip_version':strip_version})
manifest.write_text(json.dumps(record,indent=2)+'\n')
print('MORUNA_BOOT_HOST_TOOL_RETAINED',name,'raw',info.st_size,original_sha,'shipped',shipped.st_size,shipped_sha)
PY_HOST_TOOL
    }
case "$phase" in
  kd3-fixture-tools-arm|kd3-fixture-tools-gnu)
    python3 /tmp/kd3-fixture-producer.py "$token" __KD3_TARGET__ __KD3_SECONDS__
    ;;
  phase-a-tests)
    # Focused Phase A regressions only; full release CI and actual boot gates are separate.
    export PHASE_A_OUT="/proof/target/phase-a-tests/$token"
    mkdir -p /proof/target/phase-a-tests
    mkdir -m 700 "$PHASE_A_OUT"
    cat > /tmp/phase-a-record.py <<'PY_PHASE_A_RECORD'
import datetime,hashlib,json,os,pathlib,re,sys
root=pathlib.Path(os.environ['PHASE_A_OUT']);path=root/'results.json'
name,state,*argv=sys.argv[1:]
validation_error=None
record=json.loads(path.read_text()) if path.exists() else {'version':1,'phase':'phase-a-tests','inputs_census_sha256':'__INPUTS_CENSUS__','target':'aarch64-unknown-linux-gnu','toolchain':'1.98.1','jobs':1,'complete':False,'commands':[]}
if state=='start':
    if any(item['name']==name for item in record['commands']):raise SystemExit('Duplicate phase A command')
    record['commands'].append({'name':name,'argv':argv,'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'exit':None,'state':'running','test_results':[]})
elif state=='complete':
    if any(item['exit']!=0 for item in record['commands']):raise SystemExit('Phase A cannot complete with failed/incomplete command')
    record['complete']=True
else:
    item=record['commands'][-1]
    if item['name']!=name or item['state']!='running':raise SystemExit('Phase A result ordering differs')
    log=root/(name+'.log')
    if not log.is_file() or log.stat().st_size>32*1024**2:raise SystemExit('Phase A log missing/over bound')
    raw=log.read_bytes();text=raw.decode('utf-8',errors='replace')
    results=[{'outcome':outcome,'passed':int(passed),'failed':int(failed),'ignored':int(ignored)} for outcome,passed,failed,ignored in re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',text,re.MULTILINE)]
    item.update(exit=int(state),state='finished',finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),log=log.name,bytes=len(raw),sha256=hashlib.sha256(raw).hexdigest(),test_results=results)
    if int(state)==0 and 'test' in item['argv'] and sum(result['passed']+result['failed'] for result in results)==0:
        validation_error='Selected test command executed no tests: '+name
        item['validation_error']=validation_error
tmp=path.with_suffix('.new')
with tmp.open('w') as f:json.dump(record,f,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
os.replace(tmp,path)
if validation_error:raise SystemExit(validation_error)
PY_PHASE_A_RECORD
    phase_a_check() {
      local name="$1"; shift
      python3 /tmp/phase-a-record.py "$name" start "$@"
      set +e
      (set -o noclobber; "$@" > "$PHASE_A_OUT/$name.log" 2>&1)
      local code=$?
      set -e
      python3 /tmp/phase-a-record.py "$name" "$code"
      return "$code"
    }
    phase_a_check account run_cargo test -p moruna-kernel --test ct_native_account
    phase_a_check admission run_cargo test -p moruna-controller --lib admission::tests
    phase_a_check report run_cargo test -p moruna-trace --test tr_t3_pure_report
    phase_a_check events run_cargo test -p moruna-event --test records
    phase_a_check vmm run_cargo test -p moruna-vmm --all-targets --features kd3-conformance-proof
    phase_a_check host run_cargo test -p moruna-runtime --lib host:: --features native-tcg-proof
    phase_a_check build-context run_cargo test -p moruna-runtime --lib spec::tests::cloned_build_context_shares_actual_file_buffer_claims --features native-tcg-proof
    phase_a_check storage run_cargo test -p moruna-storage-format -p moruna-storage -p moruna-reactor -- --test-threads=2
    phase_a_check clippy run_cargo clippy -p moruna-kernel -p moruna-controller -p moruna-trace -p moruna-event -p moruna-vmm -p moruna-runtime -p moruna-storage-format -p moruna-storage -p moruna-reactor --all-targets --features moruna-runtime/native-tcg-proof,moruna-vmm/kd3-conformance-proof -- -D warnings
    python3 /tmp/phase-a-record.py finished complete
    ;;
  storage-tests) run_cargo test -p moruna-storage-format -p moruna-storage -p moruna-reactor --features moruna-reactor/uring -- --test-threads=2 ;;
  check-sdk) run_cargo check --keep-going -p moruna-runtime -p moruna-cost -p moruna-placement -p moruna-scheduler -p moruna-storage -p moruna-reactor -p moruna-plane-linux -p moruna-py --all-targets --features moruna-reactor/uring,moruna-py/python,moruna-py/uring ;;
  boot-host-tools)
    # Host-helper dev/debug0 artifacts, NOT optimized workload/performance evidence.
    export RUSTFLAGS="--cfg polyval_force_soft"
    unset CARGO_ENCODED_RUSTFLAGS
    mkdir -p /proof/witness-proof
    run_cargo test -p moruna-event --test records kd3_sink_ack_tests > /proof/witness-proof/sink-ack-tests.log 2>&1
    run_cargo build -p moruna-vmm --bin moruna-vhost-vsock
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/moruna-vhost-vsock moruna-vhost-vsock
    run_cargo build -p moruna-runtime --example kd3-peer
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/examples/kd3-peer kd3-peer
    run_cargo build -p moruna-runtime --example node-ci-assets
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/examples/node-ci-assets node-ci-assets
    run_cargo build -p moruna-runtime --example kd3-native-proof --features native-tcg-proof
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/examples/kd3-native-proof kd3-native-proof
    run_cargo build -p moruna-storage --bin moruna-storage-provision
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/moruna-storage-provision moruna-storage-provision
    run_cargo build -p moruna-image --bin moruna-layer --features testing
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/moruna-layer moruna-layer
    run_cargo build -p moruna-testkit --bin plane-fixture --bin kd2-fixture
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/plane-fixture plane-fixture
    retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/kd2-fixture kd2-fixture
    python3 - <<'PY_HOST_REQUIRED'
import hashlib,json,pathlib,re
record=json.loads(pathlib.Path('/proof/witness-proof/boot-host-tools.json').read_text())
required={'moruna-vhost-vsock','kd3-peer','node-ci-assets','kd3-native-proof','moruna-storage-provision','moruna-layer','plane-fixture','kd2-fixture'}
if {item['name'] for item in record['files']}!=required or len(record['files'])!=8:raise SystemExit('Required host tool set incomplete')
log=pathlib.Path('/proof/witness-proof/sink-ack-tests.log')
if re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log.read_text(),re.MULTILINE)!=[('6','0','0')]:raise SystemExit('Six sink tests must pass without failures or ignored tests')
record['focused_tests']={'command':['cargo','test','-p','moruna-event','--test','records','kd3_sink_ack_tests'],'passed':6,'failed':0,'ignored':0,'exit':0,'log':log.name,'sha256':hashlib.sha256(log.read_bytes()).hexdigest()}
pathlib.Path('/proof/witness-proof/boot-host-tools.json').write_text(json.dumps(record,indent=2)+'\n')
PY_HOST_REQUIRED
    ;;
  sdk-proof-build)
    mkdir -p /proof/witness-proof
    set +e
    run_cargo test -p moruna-event --test records kd3_sink_ack_tests > /proof/witness-proof/sink-ack-tests.log 2>&1
    sink_status=$?
    set -e
    sha256sum /proof/witness-proof/sink-ack-tests.log > /proof/witness-proof/sink-ack-tests.sha256
    test "$sink_status" -eq 0
    set +e
    run_cargo build -p moruna-runtime --example kd3-native-proof --features native-tcg-proof
    sdk_status=$?
    set -e
    if [ "$sdk_status" -eq 0 ]; then
      retain_tool /proof/target/aarch64-unknown-linux-gnu/debug/examples/kd3-native-proof kd3-native-proof
      sha256sum /proof/witness-proof/kd3-native-proof > /proof/witness-proof/sdk-proof.sha256
      python3 - <<'PY_SINK_RESULT'
import hashlib,json,pathlib,re
root=pathlib.Path('/proof/witness-proof');log=root/'sink-ack-tests.log'
counts=re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log.read_text(),re.MULTILINE)
if counts!=[('6','0','0')]:raise SystemExit('Expected exactly six sink acknowledgement tests, zero failed/ignored')
p=root/'boot-host-tools.json';record=json.loads(p.read_text());record['focused_tests']={'command':['cargo','test','-p','moruna-event','--test','records','kd3_sink_ack_tests'],'passed':6,'failed':0,'ignored':0,'exit':0,'log':log.name,'sha256':hashlib.sha256(log.read_bytes()).hexdigest()};p.write_text(json.dumps(record,indent=2)+'\n')
PY_SINK_RESULT
    fi
    test "$sdk_status" -eq 0
    ;;
  witness-tests)
    set +e
    witness_status=0
    run_cargo test -p moruna-runtime --lib host::launch_witness::tests -- --test-threads=2 || witness_status=$?
    run_cargo test -p moruna-runtime --lib host::native::tests -- --test-threads=2 || witness_status=$?
    set -e
    mkdir -p /proof/witness-proof
    python3 - <<'PY'
import pathlib,shutil,hashlib,json
root=pathlib.Path('/proof/source');target=pathlib.Path('/proof/target/aarch64-unknown-linux-gnu/debug/deps');out=pathlib.Path('/proof/witness-proof');records=[]
for p in target.glob('moruna_runtime-*'):
    if not p.is_file() or not p.stat().st_mode&0o111:continue
    with p.open('rb') as file:header=file.read(4)
    if header!=b'\x7fELF':continue
    shutil.copy2(p,out/p.name);records.append({'name':p.name,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()})
for rel in ['Cargo.lock','crates/moruna-runtime/src/host/launch_witness.rs','crates/moruna-runtime/src/host/native.rs']:
    p=root/rel;shutil.copy2(p,out/p.name);records.append({'source':rel,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()})
(out/'artifacts.json').write_text(json.dumps(records,indent=2)+'\n')
PY
    test "$witness_status" -eq 0
    ;;
  runtime-tests) run_cargo test --no-fail-fast -p moruna-runtime -p moruna-cost -p moruna-placement -p moruna-scheduler --features moruna-reactor/uring -- --test-threads=2 ;;
  check) run_cargo check --keep-going -p moruna-runtime -p moruna-cost -p moruna-placement -p moruna-scheduler -p moruna-storage -p moruna-reactor -p moruna-plane-linux --all-targets --features moruna-reactor/uring ;;
  tests) run_cargo test -p moruna-event -p moruna-cost -p moruna-storage-format -p moruna-storage -p moruna-reactor -p moruna-vmm -p moruna-plane-linux -p moruna-sandbox --features moruna-reactor/uring -- --test-threads=2 ;;
  clippy) run_cargo clippy -p moruna-runtime -p moruna-cost -p moruna-storage-format -p moruna-storage -p moruna-reactor --all-targets --features moruna-reactor/uring -- -D warnings ;;
  *) exit 2 ;;
esac
printf 'MORUNA_COMBINED_DONE %s %s\n' "$token" "$phase"
'''


def load_kd3_bundles(options, gnu):
    """Verify only explicitly supplied immutable fixture inputs; never a home mount."""
    receipt = module.private_file(options.kd3_python_bundle)
    value = json.loads(receipt.read_text())
    root = receipt.parent
    keys = ('python', 'pyarrow') if gnu else ('python_arm', 'pyarrow_arm')
    files = [receipt]
    for key in keys:
        row = value[key]
        path = module.private_file(row['path'])
        require(path.parent == root and row.get('verified') is True and path.stat().st_size == row['bytes']
                and module.sha(path) == row['sha256'], 'actual official KD3 input differs')
        files.append(path)
    component = None
    if gnu:
        path = module.private_file(options.kd3_gnu_bundle)
        component = json.loads(path.read_text())
        require(component['rust_std_version'] == '1.98.1 (48a229cea 2026-09-01)', 'GNU target/compiler metadata differs')
        files.append(path)
        for key in ('archive', 'manifest'):
            p = module.private_file(component[key])
            require(p.parent == path.parent and module.sha(p) == component[key + '_sha256'], 'actual GNU target input differs')
            files.append(p)
    fingerprints = [(p, (p.stat().st_dev, p.stat().st_ino, p.stat().st_uid, p.stat().st_size, p.stat().st_mtime_ns)) for p in files]
    return value, component, fingerprints


class CombinedRun(module.Ke2Run):
    guest_script = GUEST
    marker = 'MORUNA_COMBINED_DONE'
    lock_key = 'cargo_lock_sha256'
    lock_label = 'cargo_lock_sha256'
    runner_files = module.Ke2Run.runner_files + ('combined-linux.py', 'proof-tools.py')

    def __init__(self, options, inputs, archive, cache):
        super().__init__(options, inputs, archive, cache)
        if options.phase in ('witness-tests', 'sdk-proof-build', 'boot-host-tools', 'phase-a-tests'):
            self.memory = 6
        if options.phase in ('boot-host-tools','sdk-proof-build'):
            self.measurement_scope='ARM64 Linux host helper dev/debug0 build and custody only; no optimization/performance/boot/execution claim'
        if options.phase=='phase-a-tests':
            self.measurement_scope='Focused ARM64 Phase A unit/integration regressions and clippy only; no native boot, KD3 matrix or release acceptance claim'
        self.guest_script = GUEST.replace('__INPUTS_CENSUS__', inputs['inputs_census_sha256'])
        self.tool_bundle = tool_module.load_bundle(options.tools_bundle)
        self.attachments.append({'path': self.tool_bundle['root'],
                                 'destination': self.tool_bundle['destination'], 'readonly': True})
        self.kd3_fingerprints=[]
        if options.phase in ('kd3-fixture-tools-arm','kd3-fixture-tools-gnu'):
            gnu=options.phase.endswith('-gnu');target='x86_64-unknown-linux-gnu' if gnu else 'aarch64-unknown-linux-gnu'
            require(cache['target']==target and cache['profile']=='native-debug0','KD3 fixture target/cache must agree exactly')
            self.memory=6
            self.measurement_scope='Actual '+target+' Python/peQL fixture and library dev-debug0 build only; no matrix execution or performance claim'
            python_bundle,component,self.kd3_fingerprints=load_kd3_bundles(options,gnu)
            self.attachments.append({'path':str(Path(options.kd3_python_bundle).parent),'destination':'/inputs/kd3-python','readonly':True})
            payload={'kd3-fixture-producer.py':(ROOT/'kd3-fixture-producer.py').read_text(),'kd3-python-bundle.json':json.dumps(python_bundle)}
            self.runner_files=self.runner_files+('kd3-fixture-producer.py',)
            if gnu:
                loaded=importlib.util.spec_from_file_location('kd3_cross_inputs',ROOT/'cross-tools-vm.py')
                cm=importlib.util.module_from_spec(loaded);loaded.loader.exec_module(cm)
                cross=cm.load_inputs(options.cross_bundle, options.rust_bundle)
                self.attachments.extend([{'path':str(cross['cross_path'].parent),'destination':'/inputs/cross','readonly':True},
                                         {'path':str(Path(options.kd3_gnu_bundle).parent),'destination':'/inputs/kd3-gnu','readonly':True}])
                for name in ('cross-tools-linux-smoke.py','personality-producer.py'):
                    payload[name]=(ROOT/name).read_text()
                payload['kd3-gnu-bundle.json']=json.dumps(component)
                self.runner_files=self.runner_files+('cross-tools-linux-smoke.py','personality-producer.py','cross-tools-vm.py')
                for path in (cross['cross_path'],cross['cross_path'].parent/'manifest.json',cross['cross_path'].parent/'cross-tools.tar.gz'):
                    z=path.stat();self.kd3_fingerprints.append((path,(z.st_dev,z.st_ino,z.st_uid,z.st_size,z.st_mtime_ns)))
            encoded=base64.b64encode(json.dumps(payload).encode()).decode()
            prefix="set -euo pipefail\npython3 - <<'PY_KD3_PAYLOAD'\nimport base64,json,pathlib\nfor name,text in json.loads(base64.b64decode("+repr(encoded)+")).items():\n    pathlib.Path('/tmp',name).write_text(text)\nPY_KD3_PAYLOAD\n"
            self.guest_script=prefix+self.guest_script.replace('__KD3_TARGET__',target).replace('__KD3_SECONDS__',str(max(1,options.seconds-180)))
            if gnu:
                self.guest_script=self.guest_script.replace('--target aarch64-unknown-linux-gnu','--target x86_64-unknown-linux-gnu').replace("['--target','aarch64-unknown-linux-gnu']","['--target','x86_64-unknown-linux-gnu']")


    def call(self, arguments, *args, **kwargs):
        result = super().call(arguments, *args, **kwargs)
        if self.options.phase in ('witness-tests', 'sdk-proof-build', 'boot-host-tools') and len(arguments) > 4 and arguments[:4] == ['exec', self.name, '/bin/bash', '-c'] and arguments[4] == self.guest_script:
            require(self.owned() is not None, 'owned witness VM missing before artifact collection')
            super().call(['copy', self.name + ':/proof/witness-proof', str(self.evidence / 'witness-proof')], seconds=45, check=self.options.phase in ('boot-host-tools','sdk-proof-build'))
            if self.options.phase in ('boot-host-tools','sdk-proof-build') and result[0]==0:
                folder=self.evidence/'witness-proof'
                record=json.loads((folder/'boot-host-tools.json').read_text())
                required={'moruna-vhost-vsock','kd3-peer','node-ci-assets','kd3-native-proof','moruna-storage-provision','moruna-layer','plane-fixture','kd2-fixture'} if self.options.phase=='boot-host-tools' else {'kd3-native-proof'}
                require(record.get('profile')=='dev-debug0' and record.get('target')=='aarch64-unknown-linux-gnu' and record.get('toolchain')=='1.98.1' and record.get('jobs')==1, 'retained host-helper profile differs')
                require({item['name'] for item in record['files']}==required and len(record['files'])==len(required), 'required retained host tool missing')
                total=0;raw_total=0
                for item in record['files']:
                    path=folder/item['name'];info=path.lstat();total+=info.st_size;raw_total+=item['original_bytes']
                    require(stat.S_ISREG(info.st_mode) and info.st_mode&0o111 and 0<info.st_size<=128*1024**2 and info.st_size==item['bytes'], 'retained host tool type/size differs')
                    digest=hashlib.sha256()
                    with path.open('rb') as file:
                        header=file.read(20);file.seek(0)
                        for chunk in iter(lambda:file.read(1024**2),b''):digest.update(chunk)
                    require(header[:6]==b'\x7fELF\x02\x01' and header[18:20]==b'\xb7\x00' and digest.hexdigest()==item['sha256'], 'retained host tool ELF/hash differs')
                require(total<=1024*1024**2 and raw_total<=2*1024**3, 'retained host helper aggregate exceeds declared bound')
                require(all(0<item['original_bytes']<=512*1024**2 and len(item['original_sha256'])==64 and len(item['strip_tool_sha256'])==64 and item['strip_version'] for item in record['files']), 'raw/strip provenance missing')
        return result

    def owned(self, cleanup=False):
        # Input custody failure must never prevent nonce/config-verified teardown.
        if not cleanup:
            for path,expected in self.kd3_fingerprints:
                z=module.private_file(path).stat();require((z.st_dev,z.st_ino,z.st_uid,z.st_size,z.st_mtime_ns)==expected,'KD3 immutable input changed')
            tool_module.load_bundle(self.options.tools_bundle)
        return super().owned(cleanup=cleanup)


    def execute(self):
        result=super().execute()
        if self.options.phase in ('kd3-fixture-tools-arm','kd3-fixture-tools-gnu'):
            record={'collected':False,'files':[]}
            try:
                require(json.loads((self.evidence/'summary.json').read_text()).get('cleanup') is True,'actual cleanup required before KD3 artifact collection')
                validate_cache(self.cache,self.cache['owner_nonce'])
                source=Path(self.cache['path'])/'kd3-fixture-tools'/self.token
                require(source.is_dir() and not source.is_symlink(),'actual KD3 producer output missing')
                dest=self.evidence/'kd3-fixture-tools';dest.mkdir(mode=0o700);total=0
                for path in sorted(source.iterdir()):
                    info=path.lstat();require(stat.S_ISREG(info.st_mode) and info.st_uid==os.getuid() and info.st_nlink==1 and info.st_size<=512*1024**2,'KD3 artifact type/size/owner differs')
                    require(path.name in ('manifest.json','manifest.new','moruna-kd3-fixture','_core.so','kd3-fixture-boot','arrow_echo') or path.name.endswith('.log') or (path.name.startswith('cxx-object-') and path.name.endswith('.o')) or (path.name.startswith('cxx-inspect-') and path.name.endswith('.json')),'unexpected KD3 producer file')
                    total+=info.st_size;require(total<=2*1024**3,'KD3 aggregate test-artifact bound')
                    shutil.copy2(path,dest/path.name);record['files'].append({'name':path.name,'bytes':info.st_size,'sha256':module.sha(dest/path.name)})
                if (dest/'manifest.json').exists():
                    actual=json.loads((dest/'manifest.json').read_text());record['actual']=actual
                    if result==0:
                        require(actual.get('passed') is True and actual['target']==self.cache['target'] and actual['jobs']==1 and actual['profile']=='dev-debug0','actual KD3 producer success/profile differs')
                        required={'moruna-kd3-fixture','_core.so'} | ({'kd3-fixture-boot','arrow_echo'} if self.cache['target'].startswith('aarch64') else set())
                        require({x['name'] for x in actual['files']}==required and len(actual['files'])==len(required),'actual KD3 binary/library/boot helper missing')
                        require(all(x.get('exit')==0 for x in actual['commands']),'actual KD3 command failed')
                    for row in actual['files']:
                        artifact=dest/row['name'];require(artifact.stat().st_size==row['bytes'] and module.sha(artifact)==row['sha256'],'actual KD3 artifact SHA differs')
                        with artifact.open('rb') as f:header=f.read(20)
                        machine=b'\xb7\x00' if self.cache['target'].startswith('aarch64') else b'\x3e\x00'
                        require(header[:6]==b'\x7fELF\x02\x01' and header[18:20]==machine,'actual KD3 target ELF differs')
                    for row in actual['commands']:
                        if row.get('sha256'):require(module.sha(dest/row['log'])==row['sha256'],'actual KD3 build log SHA differs')
                    for inspect in dest.glob('cxx-inspect-*.json'):
                        diagnostic=json.loads(inspect.read_text());name=diagnostic['diagnostic_path']
                        require(Path(name).name==name and name.startswith('cxx-object-') and name.endswith('.o'),'C++ diagnostic path differs')
                        obj=dest/name;require(obj.stat().st_size==diagnostic['object_bytes'] and module.sha(obj)==diagnostic['object_sha256'] and diagnostic.get('inspected') is True,'C++ actual object inspection differs')
                        if result==0:require(diagnostic.get('accepted') is True and not diagnostic['unknown_symbols'],'C++ runtime inspection refused')
                elif result==0:raise RuntimeError('actual KD3 manifest missing')
                record['collected']=True
            except BaseException as error:record['error']=type(error).__name__+': '+str(error);result=1
            (self.evidence/'kd3-fixture-artifacts.json').write_text(json.dumps(record,indent=2)+'\n')
            return result
        if self.options.phase!='phase-a-tests':return result
        # Keep partial logs even when the guest command timed out. Only read the
        # owned cache after inherited nonce-verified VM teardown completed.
        record={'phase':'phase-a-tests','collected':False,'files':[]}
        try:
            require(json.loads((self.evidence/'summary.json').read_text()).get('cleanup') is True,'actual cleanup required before phase A log collection')
            validate_cache(self.cache,self.cache['owner_nonce'])
            source=Path(self.cache['path'])/'phase-a-tests'/self.token
            require(source.is_dir() and not source.is_symlink(),'phase A actual logs missing')
            dest=self.evidence/'phase-a-tests';dest.mkdir(mode=0o700);total=0
            for path in sorted(source.iterdir()):
                info=path.lstat();require(stat.S_ISREG(info.st_mode) and info.st_uid==os.getuid() and info.st_nlink==1 and info.st_size<=32*1024**2,'phase A log type/owner/bound differs')
                require(path.name=='results.json' or path.name=='results.new' or (path.name.endswith('.log') and path.stem in {'account','admission','report','events','vmm','host','build-context','storage','clippy'}),'unexpected phase A evidence')
                total+=info.st_size;require(total<=128*1024**2,'phase A aggregate log bound')
                shutil.copy2(path,dest/path.name);record['files'].append({'name':path.name,'bytes':info.st_size,'sha256':hashlib.sha256((dest/path.name).read_bytes()).hexdigest()})
            if (dest/'results.json').is_file():
                actual=json.loads((dest/'results.json').read_text());record['actual_results']=actual
                for command in actual['commands']:
                    if command.get('sha256'):
                        log=dest/command['log'];require(log.stat().st_size==command['bytes'] and hashlib.sha256(log.read_bytes()).hexdigest()==command['sha256'],'phase A command log differs')
                if result==0:require(actual.get('complete') is True and all(item.get('exit')==0 for item in actual['commands']),'phase A success requires actual complete zero exits')
            elif result==0:raise RuntimeError('phase A result receipt missing')
            record['collected']=True
        except BaseException as error:record['error']=type(error).__name__+': '+str(error);result=1
        (self.evidence/'phase-a-test-artifacts.json').write_text(json.dumps(record,indent=2)+'\n')
        return result


class PersonalityRun(CombinedRun):
    """The same owned VM lifecycle, with a separate actual musl producer cache."""
    runner_files = CombinedRun.runner_files + ('personality-producer.py', 'cross-tools-linux-smoke.py', 'cross-tools-vm.py')
    measurement_scope = 'x86_64 Linux personality artifact production and static ELF inspection only; no execution/boot claim'

    def __init__(self, options, inputs, archive, cache):
        require(cache['target']=='x86_64-unknown-linux-musl' and cache['profile']=='release-debug0', 'personality cache must be separate')
        require(not cache.get('requires_local_clean') or not getattr(options,'reuse_summary',None) or getattr(options,'reuse_transition',None), 'retained artifacts require the explicit source transition')
        super().__init__(options, inputs, archive, cache)
        self.memory=6
        loaded=importlib.util.spec_from_file_location('personality_cross_inputs',ROOT/'cross-tools-vm.py')
        self.cross_module=importlib.util.module_from_spec(loaded);loaded.loader.exec_module(self.cross_module)
        self.cross_inputs=self.cross_module.load_inputs(options.cross_bundle, options.rust_bundle)
        self.attachments.extend([
            {'path':str(self.cross_inputs['cross_path'].parent),'destination':'/inputs/cross','readonly':True},
            {'path':str(self.cross_inputs['rust_root']),'destination':'/inputs/musl','readonly':True}])
        payload={
            'personality-producer.py':(ROOT/'personality-producer.py').read_text(),
            'cross-tools-linux-smoke.py':(ROOT/'cross-tools-linux-smoke.py').read_text(),
            'rust-bundle.json':self.cross_inputs['rust_path'].read_text(),
            'rust-manifest.json':self.cross_inputs['rust_manifest'].read_text()}
        self.reuse_fingerprints=[]
        if getattr(options,'reuse_summary',None):
            payload['personality-reuse.json']=json.dumps(self.load_reuse(options.reuse_summary))
        elif cache.get('requires_local_clean'):
            transition_path=module.private_file(cache['previous_binding_receipt'])
            transition=json.loads(transition_path.read_text())
            require(transition.get('moved') is True and transition['to']==cache, 'full rebuild cache transition differs')
            payload['personality-full-clean.json']=json.dumps({'new_inputs_census_sha256':inputs['inputs_census_sha256'],'sha256':self.cross_module.sha(transition_path)})
        self.layers_fingerprints=[]
        if getattr(options,'layers_tests_kernel_inputs',None):
            provenance_path=module.private_file(options.layers_tests_kernel_inputs)
            layers=json.loads(provenance_path.read_text());layers_archive=module.private_file(layers['archive_path'])
            module.validate_inputs(layers,layers_archive,lock_path='kernel/Cargo.lock',lock_key='kernel_cargo_lock_sha256',max_files=20000,max_bytes=400*1024**2)
            require(layers['archive_sha256']=='16c10d15d973f3dd157164228ded1ab929bc90a127715c7d2281f66f642e2787','focused tests require exact reviewed kernel47 vendor input')
            self.attachments.append({'path':str(layers_archive.parent),'destination':'/inputs/layers-kernel','readonly':True})
            payload['layers-tests.json']=json.dumps({'archive':layers_archive.name,'sha256':layers['archive_sha256']})
            for path in (provenance_path,layers_archive):
                info=path.stat();self.layers_fingerprints.append({'path':str(path),'sha256':self.cross_module.sha(path),'device':info.st_dev,'inode':info.st_ino,'bytes':info.st_size})
        encoded=base64.b64encode(json.dumps(payload).encode()).decode()
        self.guest_script = r'''set -euo pipefail
token="$1"; phase="$2"; archive_sha="$3"
printf '%s  /tmp/moruna-source.tar\n' "$archive_sha" | sha256sum -c -
mkdir -p /proof/source /proof/cargo
tar -xf /tmp/moruna-source.tar -C /proof/source
chmod -R a-w /proof/source
cd /proof/source
export CARGO_HOME=/proof/cargo CARGO_TARGET_DIR=/proof/target CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=1
export LIBCLANG_PATH=/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu
export LD_LIBRARY_PATH=/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu:/opt/moruna-proof-tools/usr/lib/llvm-14/lib
python3 - <<'PY_PERSONALITY'
import base64,json,pathlib
for name,text in json.loads(base64.b64decode('__PAYLOAD__')).items():
    pathlib.Path('/tmp',name).write_text(text)
PY_PERSONALITY
python3 /tmp/personality-producer.py --token "$token" --seconds __SECONDS__
printf 'MORUNA_COMBINED_DONE %s %s\n' "$token" "$phase"
'''.replace('__PAYLOAD__',encoded).replace('__SECONDS__',str(max(1,options.seconds-150)))

    def load_transition(self, prior, summary_path):
        path=module.private_file(self.options.reuse_transition)
        require(self.cache.get('requires_local_clean') is True and self.cache.get('previous_binding_receipt')==str(path),'current cache lacks exact retarget requirement')
        stable=('owner_nonce','device','inode','toolchain','target','profile','destination','readonly')
        cursor=self.cache;receipt_path=path;chain=[];chain_checked=[];seen_receipts=set();hop_sources=[]
        original=None;current_provenance=None
        while cursor!=prior['cache']:
            require(len(chain)<4 and str(receipt_path) not in seen_receipts,'explicit transition chain bound/cycle')
            seen_receipts.add(str(receipt_path));transition=json.loads(receipt_path.read_text())
            require(transition.get('moved') is True and transition['to']==cursor,'receipt does not prove exact current chain binding')
            require(all(transition['from'][key]==self.cache[key] and cursor[key]==self.cache[key] for key in stable),'transition changed actual cache identity/compiler profile')
            require(cursor.get('previous_binding_receipt')==str(receipt_path),'transition pointer differs from authoritative cache binding')
            old_path=module.private_file(transition['prior_provenance']);hop_new_path=module.private_file(transition['new_provenance'])
            require(self.cross_module.sha(old_path)==transition['prior_provenance_sha256'] and self.cross_module.sha(hop_new_path)==transition['new_provenance_sha256'],'hop exact provenance SHA differs')
            old=json.loads(old_path.read_text());hop_new=json.loads(hop_new_path.read_text())
            require(old['inputs_census_sha256']==transition['from']['inputs_census_sha256'] and hop_new['inputs_census_sha256']==cursor['inputs_census_sha256'],'hop source/cache census binding differs')
            require(old['cargo_lock_sha256']==hop_new['cargo_lock_sha256']==self.inputs['cargo_lock_sha256'],'hop external package lock changed')
            if current_provenance is None:
                require(hop_new==self.inputs,'latest receipt does not name exact new source')
                current_provenance=hop_new_path
            last_path=module.private_file(transition['last_actual_cleanup_summary']);last=json.loads(last_path.read_text())
            require(self.cross_module.sha(last_path)==transition['last_actual_cleanup_summary_sha256'] and last.get('cleanup') is True and last['cache']==transition['from'],'actual hop owner cleanup missing')
            hop_manifests=[]
            for provenance in (old,hop_new):
                manifest=module.private_file(provenance['inputs_census_path'])
                require(self.cross_module.sha(manifest)==provenance['inputs_census_sha256'],'hop actual input manifest SHA differs')
                hop_manifests.append({r['path']:r for r in json.loads(manifest.read_text())});chain_checked.append(manifest)
            hop_sources.append(hop_manifests)
            chain.append({'path':str(receipt_path),'sha256':self.cross_module.sha(receipt_path),
                          'from_inputs_census_sha256':old['inputs_census_sha256'],'to_inputs_census_sha256':hop_new['inputs_census_sha256']})
            chain_checked.extend([receipt_path,old_path,hop_new_path,last_path])
            cursor=transition['from']
            if cursor==prior['cache']:
                require(self.cross_module.sha(old_path)==prior['input_manifest_sha256'] and old['archive_sha256']==prior['source_archive_sha256'] and old['inputs_census_sha256']==prior['inputs_census_sha256'],'chain did not reach original executed closed-artifact source')
                original=(old_path,old)
                break
            require(isinstance(cursor.get('previous_binding_receipt'),str),'explicit previous transition pointer missing')
            receipt_path=module.private_file(cursor['previous_binding_receipt'])
        require(original is not None and chain,'transition chain must reach original prior cache')
        old_path,old=original;new_path=current_provenance
        old_manifest=module.private_file(old['inputs_census_path']);new_manifest=module.private_file(self.inputs['inputs_census_path'])
        require(self.cross_module.sha(old_manifest)==old['inputs_census_sha256'] and self.cross_module.sha(new_manifest)==self.inputs['inputs_census_sha256'],'actual source manifests differ')
        a={r['path']:r for r in json.loads(old_manifest.read_text())};b={r['path']:r for r in json.loads(new_manifest.read_text())}
        # Reanchor to the actual passed 328 producer; only this exact account overlay.
        approved_additions={'crates/moruna-controller/src/admission.rs'}
        expected={'crates/moruna-controller/src/lib.rs': [{'bytes': 59798, 'mode': 420, 'path': 'crates/moruna-controller/src/lib.rs', 'sha256': '3902aefda74e5ce7740b89f577f410a4365362f4bf0a647b83fc164f203cee0c', 'type': 'file'}, {'path': 'crates/moruna-controller/src/lib.rs', 'type': 'file', 'bytes': 59973, 'mode': 420, 'sha256': 'c14ea10dd93b88981598260d100389c5adf6217d2d933e6540e2ddcdb3cb9909'}], 'crates/moruna-controller/src/model.rs': [{'bytes': 38651, 'mode': 420, 'path': 'crates/moruna-controller/src/model.rs', 'sha256': 'e816b2e791bc7fdb16392d22d5cf3b41a9272b4be8a3f9c665dd39b752c52248', 'type': 'file'}, {'path': 'crates/moruna-controller/src/model.rs', 'type': 'file', 'bytes': 39006, 'mode': 420, 'sha256': '6a16d047d07b2c0c0b33acbb8531ac5d8ff9769ffb86b234df41fd3c06225504'}], 'crates/moruna-runtime/src/run.rs': [{'bytes': 92367, 'mode': 420, 'path': 'crates/moruna-runtime/src/run.rs', 'sha256': 'b2585e69ae16718d4cfba2ec3e5c6f220a12f9fd943a194ca22eb877c7c8f6f2', 'type': 'file'}, {'path': 'crates/moruna-runtime/src/run.rs', 'type': 'file', 'bytes': 93089, 'mode': 420, 'sha256': '02e47097853e8ed8b867c2db176773b2fdd3735442bf616f5891bbfe4c441b2c'}], 'crates/moruna-runtime/src/native_account.rs': [{'bytes': 11172, 'mode': 420, 'path': 'crates/moruna-runtime/src/native_account.rs', 'sha256': '699c2d8b27a0d3476ffb98b85e95c175ff04f55f19d613fb0cdc576ef3baeb1c', 'type': 'file'}, {'path': 'crates/moruna-runtime/src/native_account.rs', 'type': 'file', 'bytes': 11635, 'mode': 420, 'sha256': '5042b28a4bba2ba105f8848d62f4c7811d08c61ec4945db8fb69add0b628e503'}], 'crates/moruna-controller/src/admission.rs': [None, {'path': 'crates/moruna-controller/src/admission.rs', 'type': 'file', 'bytes': 5982, 'mode': 420, 'sha256': 'c0d719bff5807b137b722f0cb5c03871663fa609d2fcdd0b414b7ce8130cab90'}]}
        require(not(set(a)-set(b)) and set(b)-set(a)==approved_additions,'unexpected source additions/removals')
        changed={n for n in a.keys()|b.keys() if a.get(n)!=b.get(n)}
        require(changed==set(expected),'source transition exceeds exact account overlay')
        for name,(before,after) in expected.items():
            require(a.get(name)==before and b.get(name)==after,'approved source record differs')
        require(len(hop_sources)==1,'passed producer reanchor requires one exact new hop')
        require(hop_sources[0]==[a,b],'actual retarget hop differs from passed anchor/final source')
        require(a['Cargo.lock']==b['Cargo.lock'] and a['.cargo/config.toml']==b['.cargo/config.toml'] and old['cargo_lock_sha256']==self.inputs['cargo_lock_sha256'],'lock/config/external pins changed')
        # Resolve conservative normal/build path dependencies from the actual
        # unchanged Cargo manifests (including target-specific dependencies).
        # Exclude dev dependencies; include optional paths conservatively.
        def closure(source, census):
            pending=[source/'crates/moruna-image/Cargo.toml'];seen=set()
            while pending:
                manifest=pending.pop().resolve(strict=True)
                if manifest in seen:continue
                require(manifest.is_relative_to(source),'Arrow authored dependency escaped source')
                if 'vendor' in manifest.relative_to(source).parts:continue
                relative=str(manifest.relative_to(source));require(relative in census and self.cross_module.sha(manifest)==census[relative]['sha256'],'Arrow manifest bytes differ from actual archive census')
                seen.add(manifest);data=tomllib.loads(manifest.read_text())
                workspace=None
                for parent in [manifest.parent,*manifest.parent.parents]:
                    candidate=parent/'Cargo.toml'
                    if candidate.is_file() and 'workspace' in tomllib.loads(candidate.read_text()):
                        relative=str(candidate.relative_to(source));require(relative in census and self.cross_module.sha(candidate)==census[relative]['sha256'],'workspace manifest bytes differ from actual archive census')
                        workspace=parent;break
                require(workspace is not None,'actual workspace manifest missing')
                inherited=tomllib.loads((workspace/'Cargo.toml').read_text())['workspace'].get('dependencies',{})
                groups=[data.get(k,{}) for k in ('dependencies','build-dependencies')]
                for target in data.get('target',{}).values():groups.extend(target.get(k,{}) for k in ('dependencies','build-dependencies'))
                for group in groups:
                    for name,dep in group.items():
                        if not isinstance(dep,dict):continue
                        base=manifest.parent
                        if dep.get('workspace'):dep=inherited[name];base=workspace
                        if isinstance(dep,dict) and 'path' in dep:pending.append(base/dep['path']/'Cargo.toml')
            return sorted(str(p.parent.relative_to(source)) for p in seen)
        old_source=old_path.parent/'source';new_source=new_path.parent/'source'
        roots=closure(old_source,a);require(roots==closure(new_source,b),'Arrow actual authored path dependency graph changed')
        require(roots==['crates/moruna-image','crates/moruna-plane-linux','kernel/abi','kernel/imagefs/fs','kernel/plane/check','kernel/plane/core','kernel/sys'],'unexpected conservative authored Arrow closure')
        for name in a:
            if name.startswith('kernel/linux/programs/') or any(name.startswith(root+'/') for root in roots):
                require(a[name]==b[name],'closed C/Arrow authored dependency changed')
        checked=[*chain_checked,old_manifest,new_manifest]
        return {'path':str(path),'sha256':self.cross_module.sha(path),'from':prior['cache'],'to':self.cache,'chain':chain,
                'old_inputs_census_sha256':old['inputs_census_sha256'],'new_inputs_census_sha256':self.inputs['inputs_census_sha256'],
                'approved_delta':expected,'unchanged_arrow_authored_roots':roots},checked

    def load_reuse(self, raw):
        summary_path=module.private_file(raw);prior=json.loads(summary_path.read_text())
        require(summary_path.name=='summary.json' and summary_path.parent.name==prior['name'],'explicit prior evidence identity differs')
        require(prior.get('cleanup') is True and prior.get('phase')=='personality-artifacts' and prior.get('stage_passed') is True and prior.get('guest_exit')==0 and prior.get('nonce')=='328247e2d003072233a64c0be260d634','exact passed producer anchor missing')
        transition=None;transition_files=[]
        if getattr(self.options,'reuse_transition',None):
            transition,transition_files=self.load_transition(prior,summary_path)
        else:
            require(prior['cache']==self.cache,'reuse requires SAME explicitly supplied cache object/profile')
            require(prior['source_archive_sha256']==self.inputs['archive_sha256'],'prior exact source archive differs')
            for key in ('cargo_lock_sha256','source_census_sha256','inputs_census_sha256'):
                require(prior[key]==self.inputs[key],'prior source/compiler cache binding differs: '+key)
        require(prior['scripts']['personality-producer.py']=='6ec389b262289342891674a7ff14fe14604f3c7456278b2973fc9478ba2eef4f' and prior['scripts']['combined-linux.py']=='ad1825cd3801e2241df120365ea1a885b89868c7c22c62a90f8c059aaa57992f'
                and prior['scripts']['cross-tools-linux-smoke.py']=='d2aa13f7d21d699a730e85ba0647e9394407cc798c6c4ffbf1c27d9b2f1a6e84'
                and prior['scripts']['cross-tools-vm.py']=='ce3f0aaceee3819206fae507174e0769f875cd241001d2f0eeba8d28f4786ac2','prior accepted producer/verifier source differs')
        nonce=prior['nonce'];require(len(nonce)==32 and all(c in '0123456789abcdef' for c in nonce),'prior nonce differs')
        cross=self.cross_inputs
        old_tools=prior['readonly_tools']
        require(tool_module.load_bundle(str(Path(old_tools['root']).parent/'bundle.json'))==old_tools,'prior verified tool input changed')
        expected=[prior['cache'],{'path':old_tools['root'],'destination':old_tools['destination'],'readonly':True},
                  {'path':str(cross['cross_path'].parent),'destination':'/inputs/cross','readonly':True},
                  {'path':str(cross['rust_root']),'destination':'/inputs/musl','readonly':True}]
        expected.append({'path':str((ROOT/'kernel-inputs-47otixg0').resolve()),'destination':'/inputs/layers-kernel','readonly':True})
        inspect_path=None
        for path in sorted(summary_path.parent.glob('*-inspect.log')):
            path=module.private_file(path)
            try:records=json.loads(path.read_text())
            except ValueError:continue
            if not isinstance(records,list) or len(records)!=1:continue
            entry=records[0];config=entry.get('configuration',{})
            if entry.get('id')!=prior['name']:continue
            require(config.get('labels',{}).get(module.LABEL)==nonce and config['image']['descriptor']['digest']==prior['pins']['index']==self.pins['index'],'prior VM identity/base differs')
            require(config['platform']=={'os':'linux','architecture':'arm64'} and config['resources']['memoryInBytes']==6*1024**3,'prior real VM architecture/memory differs')
            archived_mounts=[{'source':item['path'],'destination':item['destination'],'type':{'virtiofs':{}},'options':['ro'] if item['readonly'] else []} for item in expected]
            require(sorted(config.get('mounts',[]),key=lambda x:x['destination'])==sorted(archived_mounts,key=lambda x:x['destination']),'prior archived actual mounts differ from original cache binding')
            inspect_path=path;break
        require(inspect_path is not None,'actual prior readonly cross/musl/cache mount evidence missing')
        root=summary_path.parent
        progress_path=module.private_file(root/'personality-proof/progress.json')
        progress=json.loads(progress_path.read_text())
        collection_path=module.private_file(root/'personality-artifacts.json');collection=json.loads(collection_path.read_text())
        require(collection.get('collected') is True,'prior retained output custody missing')
        names={'hello','threads','refuse','isolate','bench','granted','hog','tap','badrecord','dynamic','arrow_echo'}
        artifacts={a['name']:a for a in progress['artifacts']}
        require(progress.get('passed') is True and names < set(artifacts) and set(artifacts)==names|{'customlabels-preflight.o','moruna-engine','transform.py','arrowlite.py','greeting.txt','relocated-musl.specs'} and len(progress['artifacts'])==17,'passed producer actual artifact set differs')
        retained={a['name']:a for a in collection['files']}
        originals=Path(self.cache['path'])/'personality-artifacts'/nonce
        checked=[summary_path,progress_path,collection_path,inspect_path,*transition_files]
        # Exactly three real producer links; neither names nor equal hashes invent a run.
        lineage=progress['reused_from']
        for expected_nonce in ['f2bc136746eaec7f0447d4df7643681d', '0c76b3d7ff80d69d11ffb756f3f423e1', '471cd1d36b24253cc5b2793bf0375e92']:
            origin_summary=module.private_file(lineage['summary']);origin=json.loads(origin_summary.read_text())
            origin_progress=module.private_file(origin_summary.parent/'personality-proof/progress.json')
            require(origin['nonce']==expected_nonce and origin.get('cleanup') is True and self.cross_module.sha(origin_summary)==lineage['summary_sha256'] and self.cross_module.sha(origin_progress)==lineage['progress_sha256'],'closed C11 producer link differs')
            origin_data=json.loads(origin_progress.read_text());origin_records={item['name']:item for item in origin_data['artifacts']}
            if expected_nonce!='471cd1d36b24253cc5b2793bf0375e92':
                require(origin.get('stage_passed') is True and origin.get('guest_exit')==0 and origin_data.get('passed') is True,'intermediate producer did not actually pass')
            origin_inspects=[module.private_file(path) for path in origin_summary.parent.glob('*-inspect.log') if self.cross_module.sha(path)==lineage['inspect_sha256']]
            require(len(origin_inspects)==1,'actual producer inspect custody missing')
            origin_collection=module.private_file(origin_summary.parent/'personality-artifacts.json')
            origin_collection_data=json.loads(origin_collection.read_text());require(origin_collection_data.get('collected') is True,'historical producer collection incomplete')
            original_retained={item['name']:item for item in origin_collection_data['files']}
            checked.extend([origin_summary,origin_progress,origin_collection,*origin_inspects])
            for name in names:
                require(all(artifacts[name][key]==origin_records[name][key] for key in ('name','bytes','sha256','elf','elf_verified','dynamic')),'C11 historical artifact metadata differs')
                require(original_retained[name]['bytes']==artifacts[name]['bytes'] and original_retained[name]['sha256']==artifacts[name]['sha256'],'C11 historical collection differs')
                origin_file=module.private_file(origin_summary.parent/'personality-proof'/name)
                require(origin_file.stat().st_size==artifacts[name]['bytes'] and self.cross_module.sha(origin_file)==artifacts[name]['sha256'],'C11 historical retained bytes differ')
                checked.append(origin_file)
            lineage=origin_data.get('reused_from')
        require(lineage is None,'unexpected additional producer lineage')
        selected=[]
        for name in sorted(names):
            item=artifacts[name]
            require(item.get('elf') is True and item.get('elf_verified') is True and item.get('dynamic')==(name=='dynamic'),'prior actual ELF inspection missing')
            require(retained[name]['sha256']==item['sha256'] and retained[name]['bytes']==item['bytes'],'prior collection and actual artifact disagree')
            for path in [root/'personality-proof'/name,originals/name]:
                path=module.private_file(path);require(path.stat().st_size==item['bytes'] and self.cross_module.sha(path)==item['sha256'],'prior actual bytes changed')
                checked.append(path)
            selected.append(item)
        self.reuse_fingerprints=[{'path':str(path),'sha256':self.cross_module.sha(path),'device':path.stat().st_dev,'inode':path.stat().st_ino,'bytes':path.stat().st_size} for path in checked]
        return {'summary':str(summary_path),'summary_sha256':self.cross_module.sha(summary_path),'progress_sha256':self.cross_module.sha(progress_path),
                'inspect_sha256':self.cross_module.sha(inspect_path),'prior_nonce':nonce,'guest_directory':'/proof/target/personality-artifacts/'+nonce,
                'source_census_sha256':self.inputs['source_census_sha256'],'inputs_census_sha256':self.inputs['inputs_census_sha256'],'cross_archive_sha256':cross['cross']['archive_sha256'],
                'cross_manifest_sha256':cross['cross']['manifest_sha256'],'rust_archive_sha256':cross['rust']['source_archive_sha256'],
                'rust_manifest_sha256':cross['rust']['manifest_sha256'],'cache':self.cache,'transition':transition,'artifacts':selected}

    def owned(self, cleanup=False):
        for item in ([] if cleanup else getattr(self,'layers_fingerprints',[]))+getattr(self,'reuse_fingerprints',[]):
            path=module.private_file(item['path']);info=path.stat()
            require((info.st_dev,info.st_ino,info.st_size)==(item['device'],item['inode'],item['bytes']) and self.cross_module.sha(path)==item['sha256'],'prior reused artifact custody changed')
        if hasattr(self,'cross_inputs'):
            for item in self.cross_inputs['fingerprints']:
                path=self.cross_module.private_file(item['path']);info=path.stat()
                require((info.st_dev,info.st_ino,info.st_size,info.st_mtime_ns)==(item['device'],item['inode'],item['bytes'],item['mtime_ns'])
                        and self.cross_module.sha(path)==item['sha256'],'personality immutable input changed')
        return super().owned(cleanup=cleanup)

    def execute(self):
        result=super().execute()
        # Completed outputs/logs are atomically checkpointed into the exact owned
        # cache, so timeout/guest failure cannot erase partial producer progress.
        # Collect only after the inherited owned VM cleanup has finished.
        record={'scope':self.measurement_scope,'files':[],'collected':False}
        try:
            require(json.loads((self.evidence/'summary.json').read_text()).get('cleanup') is True,'owned VM cleanup must finish before host artifact collection')
            validate_cache(self.cache,self.cache['owner_nonce'])
            self.cross_module.load_inputs(self.options.cross_bundle,self.options.rust_bundle)
            source=Path(self.cache['path'])/'personality-artifacts'/self.token
            dest=self.evidence/'personality-proof';dest.mkdir(mode=0o700)
            total=0
            if source.exists():
                require(not source.is_symlink() and source.is_dir(),'personality progress path changed')
                for path in sorted(source.iterdir()):
                    if path.name.endswith('.tmp'):continue
                    info=path.lstat()
                    require(stat.S_ISREG(info.st_mode) and info.st_uid==os.getuid() and info.st_nlink==1 and info.st_size<=128*1024**2,'personality output type/bound differs')
                    total+=info.st_size;require(total<=256*1024**2,'personality retained aggregate bound')
                    shutil.copy2(path,dest/path.name)
                    record['files'].append({'name':path.name,'bytes':info.st_size,'sha256':hashlib.sha256((dest/path.name).read_bytes()).hexdigest()})
            record['collected']=True
        except BaseException as error:
            record['error']=type(error).__name__+': '+str(error);result=1
        (self.evidence/'personality-artifacts.json').write_text(json.dumps(record,indent=2)+'\n')
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--phase', choices=('check', 'check-sdk', 'witness-tests', 'sdk-proof-build', 'boot-host-tools', 'personality-artifacts', 'runtime-tests', 'storage-tests', 'tests', 'clippy', 'phase-a-tests', 'kd3-fixture-tools-arm', 'kd3-fixture-tools-gnu'), default='check')
    parser.add_argument('--inputs', required=True)
    parser.add_argument('--archive', required=True)
    parser.add_argument('--cache-journal')
    parser.add_argument('--reuse-summary')
    parser.add_argument('--reuse-transition')
    parser.add_argument('--tools-bundle', required=True)
    parser.add_argument('--kd3-python-bundle')
    parser.add_argument('--kd3-gnu-bundle')
    parser.add_argument('--cross-bundle', default=str(ROOT/'cross-tools-archive-1u_y3fit/bundle.json'))
    parser.add_argument('--rust-bundle', default=str(ROOT/'rust-musl-component/musl-target-vgegt4ot/bundle.json'))
    parser.add_argument('--seconds', type=int, default=120)
    options = parser.parse_args()
    options.mode = 'combined-linux'
    maximum=1050 if options.phase=='personality-artifacts' else 1100 if options.phase in ('witness-tests', 'sdk-proof-build', 'boot-host-tools', 'phase-a-tests', 'kd3-fixture-tools-arm', 'kd3-fixture-tools-gnu') else 600
    require(30 <= options.seconds <= maximum, 'Combined stage exceeds its explicit phase bound')
    inputs = json.loads(module.private_file(options.inputs).read_text())
    archive = module.private_file(options.archive)
    module.validate_inputs(inputs, archive, lock_path='Cargo.lock', lock_key='cargo_lock_sha256',
                           max_files=50000, max_bytes=800 * 1024 * 1024)
    if options.phase in ('kd3-fixture-tools-arm','kd3-fixture-tools-gnu'):
        require(options.kd3_python_bundle and (not options.phase.endswith('-gnu') or options.kd3_gnu_bundle),'explicit official Python/GNU target bundle required')
    if not options.run:
        print(json.dumps({'prepared_only': True, 'phase': options.phase, 'source_archive_sha256': inputs['archive_sha256']}))
        return 0
    os.umask(0o077)
    target,profile=('x86_64-unknown-linux-musl','release-debug0') if options.phase=='personality-artifacts' else ('x86_64-unknown-linux-gnu' if options.phase=='kd3-fixture-tools-gnu' else 'aarch64-unknown-linux-gnu','native-debug0')
    if options.cache_journal:
        cache = json.loads(module.private_file(options.cache_journal).read_text())
        validate_cache(cache, cache['owner_nonce'])
        require(cache['source_lock_sha256'] == inputs['cargo_lock_sha256']
                and cache.get('inputs_census_sha256') == inputs['inputs_census_sha256']
                and cache['toolchain'] == '1.98.1' and cache['target'] == target
                and cache['profile'] == profile, 'cache differs from exact combined inputs')
    else:
        cache = create_cache(secrets.token_hex(16), inputs['cargo_lock_sha256'], '1.98.1',
                             target, profile,
                             inputs_sha256=inputs['inputs_census_sha256'])
    cls=PersonalityRun if options.phase=='personality-artifacts' else CombinedRun
    run = cls(options, inputs, archive, cache)
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
