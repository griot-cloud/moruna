#!/usr/bin/env python3
"""Bounded actual bare-metal target compilation, never a boot or deployment claim."""
import argparse,importlib.util,json,os,secrets,signal,sys
from pathlib import Path
from attachments import create_cache,validate_cache
from run import ROOT,require

spec=importlib.util.spec_from_file_location('native_ke2',ROOT/'ke2-linux.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
tool_spec=importlib.util.spec_from_file_location('native_tools',ROOT/'proof-tools.py');tools=importlib.util.module_from_spec(tool_spec);tool_spec.loader.exec_module(tools)

GUEST=r'''set -euo pipefail
token="$1"; phase="$2"; archive_sha="$3"
printf 'MORUNA_NATIVE_CHECK_BEGIN %s\n' "$token"
printf '%s  /tmp/moruna-source.tar\n' "$archive_sha" | sha256sum -c -
mkdir -p /proof/source /proof/cargo
tar -xf /tmp/moruna-source.tar -C /proof/source
cd /proof/source
export CARGO_HOME=/proof/cargo CARGO_TARGET_DIR=/proof/target CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2
export PATH=/opt/moruna-proof-tools/usr/bin:/opt/moruna-proof-tools/usr/lib/llvm-14/bin:$PATH
export LD_LIBRARY_PATH=/opt/moruna-proof-tools/usr/lib/aarch64-linux-gnu:/opt/moruna-proof-tools/usr/lib/llvm-14/lib
export CC_x86_64_unknown_none=/opt/moruna-proof-tools/usr/lib/llvm-14/bin/clang
export CC_x86_64_moruna_user="$CC_x86_64_unknown_none"
export AR_x86_64_moruna_user=/usr/bin/ar
export CFLAGS_x86_64_moruna_user="--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector"
export AR_x86_64_unknown_none=/usr/bin/ar
export CFLAGS_x86_64_unknown_none='--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector'
"$CC_x86_64_unknown_none" --version
cat >/proof/compiler-smoke.c <<'C'
#include <stddef.h>
#include <stdint.h>
uint64_t moruna_actual_cross_compiler_probe(size_t value) { return (uint64_t)value + 1; }
C
"$CC_x86_64_unknown_none" --target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector -c /proof/compiler-smoke.c -o /proof/compiler-smoke.o
python3 - <<'PY'
import pathlib
header=pathlib.Path('/proof/compiler-smoke.o').read_bytes()[:20]
if header[:6]!=b'\x7fELF\x02\x01' or header[18:20]!=b'\x3e\x00':raise SystemExit('Actual cross-compiler output is not x86_64 ELF')
print('MORUNA_ACTUAL_X86_64_C_COMPILER_OK')
PY
rustup run nightly-2026-07-21 rustc -vV
python3 - <<'PY'
import hashlib,pathlib,subprocess
root=pathlib.Path(subprocess.check_output(['rustup','run','nightly-2026-07-21','rustc','--print','sysroot'],text=True).strip())
lock=root/'lib/rustlib/src/rust/library/Cargo.lock'
if hashlib.sha256(lock.read_bytes()).hexdigest()!='__RUST_SRC_LOCK_SHA256__':raise SystemExit('Actual nightly rust-src lock differs from verified dependency inputs')
print('MORUNA_NATIVE_RUST_SRC_LOCK_VERIFIED')
PY
unset CARGO_ENCODED_RUSTFLAGS
export RUSTFLAGS='--cfg polyval_force_soft -C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y'
# Deterministic archive timestamps require explicit invalidation after a source
# transition. Reuse only a receipt bound to the actually verified source archive.
MORUNA_NATIVE_ARCHIVE_SHA="$archive_sha" python3 - <<'NATIVE_CLEAN'
import os,pathlib,subprocess,json
archive=os.environ['MORUNA_NATIVE_ARCHIVE_SHA']
stamp=pathlib.Path('/proof/target')/('.moruna-native-authored-clean-'+archive+'.json')
expected={'source_archive_sha256':archive,'policy':'authored packages: host+builtin-kernel+canonical-user'}
if stamp.exists():
    if json.loads(stamp.read_text())!=expected:raise SystemExit('native clean receipt differs')
    print('MORUNA_NATIVE_AUTHORED_CLEAN_REUSED',archive)
else:
    base=['rustup','run','nightly-2026-07-21','cargo','-Zunstable-options','-Zjson-target-spec']
    metadata=json.loads(subprocess.check_output(base+['metadata','--locked','--offline','--format-version','1','--manifest-path','kernel/Cargo.toml'],text=True))
    packages=sorted(p['id'] for p in metadata['packages'] if p['source'] is None)
    if not packages:raise SystemExit('no authored native packages found')
    selection=[x for package in packages for x in ['--package',package]]
    clean=base+['clean','--locked','--offline','--release','--manifest-path','kernel/Cargo.toml']
    env=dict(os.environ);env['RUSTFLAGS']=env.get('RUSTFLAGS','')+' -Zunstable-options'
    subprocess.run(clean+selection,check=True,env=env)
    for target in ['x86_64-unknown-none','/proof/source/kernel/targets/x86_64-moruna-user.json']:
        subprocess.run(clean+['--target',target]+selection,check=True,env=env)
    stamp.write_text(json.dumps(expected)+'\n')
    print('MORUNA_NATIVE_AUTHORED_INVALIDATED',len(packages),archive)
NATIVE_CLEAN
native_target=x86_64-unknown-none
cargo_check() {
  rustup run nightly-2026-07-21 cargo check --keep-going --release --locked --offline \
    --manifest-path kernel/Cargo.toml -Zunstable-options -Zjson-target-spec --target "$native_target" \
    -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
    --config profile.release.debug=0 --config profile.release.incremental=false -j2 "$@"
}
set +e
cargo_check -p moruna-microkernel-boot --features probe
kernel_status=$?
# User processes have real per-thread SIMD state saved/restored by the existing
# OSTD hooks. Keep the kernel target flags unchanged; Ring requires SSE/SSE2 here.
native_target=kernel/targets/x86_64-moruna-user.json
export RUSTFLAGS="$RUSTFLAGS -Zunstable-options"
cargo_check --workspace --exclude moruna-microkernel --exclude moruna-microkernel-boot --exclude moruna-image --exclude moruna-node-image --features moruna-storage-service/service
userland_status=$?
set -e
printf 'MORUNA_NATIVE_CHECK_STATUSES kernel=%s userland=%s\n' "$kernel_status" "$userland_status"
test "$kernel_status" -eq 0 && test "$userland_status" -eq 0
printf 'MORUNA_NATIVE_CHECK_DONE %s %s\n' "$token" "$phase"
'''

class NativeRun(module.Ke2Run):
    guest_script=GUEST
    marker='MORUNA_NATIVE_CHECK_DONE'
    measurement_scope='Actual x86_64-unknown-none cross compilation inside owned ARM Linux VM; no linking, native execution, boot, deployment or timing proof.'
    runner_files=module.Ke2Run.runner_files+('native-check.py','proof-tools.py')
    def __init__(self,options,inputs,archive,cache):
        super().__init__(options,inputs,archive,cache)
        source_lock=inputs['stdlib_provenance']['actual_rust_src_lock_sha256']
        require(len(source_lock)==64 and all(c in '0123456789abcdef' for c in source_lock),'invalid actual rust-src digest')
        self.guest_script=GUEST.replace('__RUST_SRC_LOCK_SHA256__',source_lock)
        self.tool_bundle=tools.load_bundle(options.tools_bundle)
        require(self.tool_bundle.get('clang_executable')=='usr/lib/llvm-14/bin/clang','actual compiler input required')
        self.attachments.append({'path':self.tool_bundle['root'],'destination':self.tool_bundle['destination'],'readonly':True})
    def owned(self,cleanup=False):
        tools.load_bundle(self.options.tools_bundle)
        return super().owned(cleanup=cleanup)

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run',action='store_true');parser.add_argument('--inputs',required=True)
    parser.add_argument('--archive',required=True);parser.add_argument('--tools-bundle',required=True)
    parser.add_argument('--cache-journal');parser.add_argument('--seconds',type=int,default=600)
    options=parser.parse_args();options.mode='native-check';options.phase='check'
    require(60<=options.seconds<=600,'native stage must be bounded60..600seconds')
    inputs=json.loads(module.private_file(options.inputs).read_text());archive=module.private_file(options.archive)
    module.validate_inputs(inputs,archive)
    require(inputs.get('stdlib_provenance',{}).get('complete') is True,'actual complete rust-src dependency input required')
    if not options.run:
        print(json.dumps({'prepared_only':True,'archive_sha256':inputs['archive_sha256'],'target':'x86_64-unknown-none'}));return 0
    os.umask(0o077)
    if options.cache_journal:
        cache=json.loads(module.private_file(options.cache_journal).read_text());validate_cache(cache,cache['owner_nonce'])
        require(cache['source_lock_sha256']==inputs['kernel_cargo_lock_sha256'] and cache['inputs_census_sha256']==inputs['inputs_census_sha256'],'native cache input differs')
    else:
        cache=create_cache(secrets.token_hex(16),inputs['kernel_cargo_lock_sha256'],'nightly-2026-07-21','x86_64-unknown-none','check-release0',inputs_sha256=inputs['inputs_census_sha256'])
    run=NativeRun(options,inputs,archive,cache);run.archive(str(archive))
    def interrupted(signum,frame):raise RuntimeError('interrupted '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:run.acquire()
    except RuntimeError as error:print(str(error),file=sys.stderr);return 75
    return run.execute()

if __name__=='__main__':sys.exit(main())
