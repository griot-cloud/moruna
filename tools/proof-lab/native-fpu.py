#!/usr/bin/env python3
"""Link the actual native graph and boot only its bounded SIMD isolation fixture."""
import importlib.util
from pathlib import Path
from run import ROOT,require
spec=importlib.util.spec_from_file_location('fpu_native',ROOT/'native-check.py');native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
module=native.module

GUEST=native.GUEST[:native.GUEST.index('native_target=x86_64-unknown-none')]+r'''
mkdir -p /proof/boot-fpu
native_build() {
  rustup run nightly-2026-07-21 cargo build --keep-going --release --locked --offline \
    --manifest-path kernel/Cargo.toml --target "$native_target" \
    -Zunstable-options -Zjson-target-spec -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
    --config profile.release.debug=0 --config profile.release.incremental=false -j2 "$@"
}
native_target=x86_64-unknown-none
native_build -p moruna-microkernel-boot --features probe
export RUSTFLAGS="$RUSTFLAGS -Zunstable-options -C link-arg=-T/proof/source/kernel/sys/user.ld -C link-arg=-static"
native_target=kernel/targets/x86_64-moruna-user.json
native_build -p moruna-kb1-programs --bin fpu-a --bin fpu-b
# Build the real image writer for this ARM proof host, with host flags only.
export RUSTFLAGS='--cfg polyval_force_soft'
rustup run nightly-2026-07-21 cargo build --locked --offline --release --manifest-path kernel/Cargo.toml \
  --target aarch64-unknown-linux-gnu --config profile.release.debug=0 --config profile.release.incremental=false -p moruna-image -j2
# Derive only the registered pair from the real boot description; do not duplicate
# the success lines, process names, handles, links or exit requirements.
python3 - <<'CODE'
from pathlib import Path
text=Path('kernel/initramfs/boot.image').read_text()
marker='# Real SSE2/x87 isolation:'
start=text.index(marker); pair=text[start:]
assert pair.count('process fpu-')==2 and pair.count('  exits 0')==2
Path('/proof/boot-fpu/fixture.image').write_text('moruna-image 1\n'+pair)
CODE
/proof/target/aarch64-unknown-linux-gnu/release/moruna-image \
  /proof/boot-fpu/fixture.image /proof/target/x86_64-moruna-user/release /proof/boot-fpu
cp /proof/target/x86_64-unknown-none/release/moruna-microkernel /proof/boot-fpu/moruna-microkernel.elf
cp /proof/target/x86_64-moruna-user/release/fpu-a /proof/target/x86_64-moruna-user/release/fpu-b /proof/boot-fpu/
python3 - <<'CODE'
from pathlib import Path
p=Path('/proof/boot-fpu/moruna-microkernel.elf');data=bytearray(p.read_bytes())
assert data[:4]==b'\x7fELF' and data[18:20]==b'\x3e\x00'
data[18:20]=b'\x03\x00';Path('/proof/boot-fpu/moruna-microkernel.qemu.elf').write_bytes(data)
CODE
rustup target list --installed --toolchain 1.98.1 | tee /proof/boot-fpu/installed-stable-targets.txt
sha256sum /proof/boot-fpu/*.elf /proof/boot-fpu/fpu-a /proof/boot-fpu/fpu-b /proof/boot-fpu/initramfs.cpio
for cpus in 1 2 4; do
  set +e
  timeout 100 qemu-system-x86_64 -accel tcg -cpu max -machine q35 -m 1G -smp "$cpus" \
    -nographic -display none -monitor none -serial stdio -no-reboot \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -kernel /proof/boot-fpu/moruna-microkernel.qemu.elf \
    -initrd /proof/boot-fpu/initramfs.cpio >"/proof/boot-fpu/serial-$cpus.log" 2>&1
  status=$?
  set -e
  printf '%s\n' "$status" >"/proof/boot-fpu/status-$cpus"
  cat "/proof/boot-fpu/serial-$cpus.log"
  python3 - "$cpus" "$status" <<'CODE'
import sys,pathlib
cpus,status=map(int,sys.argv[1:]);out=pathlib.Path('/proof/boot-fpu');text=(out/f'serial-{cpus}.log').read_text(errors='replace')
assert status==33, f'Actual QEMU exit {status}, expected 33'
expected=(out/'expect').read_text().splitlines()+[f'kk: {cpus} CPUs online',f'kk: process threads ran on {cpus} of {cpus} CPUs']
for line in expected:
 assert line and line in text, f'Missing actual native line: {line}'
assert not any(x in text.lower() for x in ['fail:','panic','kk: boot refused','kk: accounting broken'])
print(f'MORUNA_NATIVE_FPU_BOOT_OK cpus={cpus}')
CODE
done
printf 'MORUNA_NATIVE_FPU_DONE %s %s\n' "$token" "$phase"
'''

class NativeFpuRun(native.NativeRun):
    guest_script=GUEST
    marker='MORUNA_NATIVE_FPU_DONE'
    measurement_scope='Actual kernel and two-process, sixteen-worker SIMD/FPU fixture link and boot under QEMU TCG on1/2/4 CPUs. Full combined application/storage/network link and boot, and production KVM admission, are separate gates.'
    runner_files=native.NativeRun.runner_files+('native-fpu.py',)
    def __init__(self,options,inputs,archive,cache):
        super().__init__(options,inputs,archive,cache)
        self.guest_script=GUEST.replace('__RUST_SRC_LOCK_SHA256__',inputs['stdlib_provenance']['actual_rust_src_lock_sha256'])
    def call(self,arguments,*args,**kwargs):
        result=super().call(arguments,*args,**kwargs)
        if len(arguments)>4 and arguments[:4]==['exec',self.name,'/bin/bash','-c'] and arguments[4]==self.guest_script:
            require(self.owned() is not None,'owned native fixture VM missing before artifact collection')
            super().call(['copy',self.name+':/proof/boot-fpu',str(self.evidence/'boot-fpu')],seconds=60,check=False)
        return result
