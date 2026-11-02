"""Compile and link the actual native graph; retain exact executable artifacts."""
import importlib.util
import hashlib,json,os,stat
from pathlib import Path
from run import ROOT,require
spec=importlib.util.spec_from_file_location('link_native',ROOT/'native-check.py');native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
module=native.module
GUEST=native.GUEST[:native.GUEST.index('native_target=x86_64-unknown-none')]+r'''
mkdir -p /proof/native-link
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
native_build --workspace --exclude moruna-microkernel --exclude moruna-microkernel-boot --exclude moruna-image --exclude moruna-node-image --features moruna-storage-service/service
python3 - <<'CODE'
from pathlib import Path
import hashlib,json,shutil
out=Path('/proof/native-link');records=[]
for target in ['x86_64-unknown-none','x86_64-moruna-user']:
 destination=out/target;destination.mkdir()
 for p in sorted((Path('/proof/target')/target/'release').iterdir()):
  if not p.is_file() or p.is_symlink() or not p.stat().st_mode&0o111:continue
  with p.open('rb') as f:header=f.read(20)
  if header[:4]!=b'\x7fELF':continue
  assert header[18:20]==b'\x3e\x00'
  shutil.copy2(p,destination/p.name)
  records.append({'target':target,'name':p.name,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()})
assert any(r['name']=='netstack' for r in records)
assert any(r['name']=='moruna-microkernel' for r in records)
assert len(records)<200 and sum(r['bytes'] for r in records)<128*1024**2
(out/'artifacts.json').write_text(json.dumps(records,indent=2)+'\n')
print('MORUNA_ACTUAL_NATIVE_EXECUTABLES',len(records))
CODE
for file in /proof/target/x86_64-moruna-user/release/build/ring-*/out/*mont*elf.o; do
  test ! -f "$file" || readelf --symbols "$file"
done
printf 'MORUNA_NATIVE_LINK_DONE %s %s\n' "$token" "$phase"
'''
ORIGIN_GUEST=r"""
# Explicit optional registered permission fixture. No product/source changes.
printf '%s  /proof/origin-proof/moruna-image\n' '__IMAGE_TOOL_SHA__' | sha256sum -c -
chmod 500 /proof/origin-proof/moruna-image
cp kernel/node/proof/origin.image /proof/origin-proof/fixture.image
/proof/origin-proof/moruna-image /proof/origin-proof/fixture.image /proof/target/x86_64-moruna-user/release /proof/origin-proof
cp /proof/target/x86_64-unknown-none/release/moruna-microkernel /proof/origin-proof/moruna-microkernel.elf
cp /proof/target/x86_64-moruna-user/release/node-memory-proof /proof/origin-proof/
python3 - <<'ORIGIN_ADAPT'
from pathlib import Path
p=Path('/proof/origin-proof/moruna-microkernel.elf');data=bytearray(p.read_bytes())
assert data[:6]==b'\x7fELF\x02\x01' and data[18:20]==b'\x3e\x00','actual original x86 kernel required'
data[18:20]=b'\x03\x00';Path('/proof/origin-proof/moruna-microkernel.qemu.elf').write_bytes(data)
ORIGIN_ADAPT
/opt/qemu/bin/qemu-system-x86_64 --version >/proof/origin-proof/qemu-version.txt
sha256sum /opt/qemu/bin/qemu-system-x86_64 >/proof/origin-proof/qemu.sha256
set +e
timeout 100 /opt/qemu/bin/qemu-system-x86_64 -accel tcg -cpu max -machine q35 -m 1G -smp 1 \
 -nographic -display none -monitor none -serial stdio -no-reboot \
 -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
 -kernel /proof/origin-proof/moruna-microkernel.qemu.elf -initrd /proof/origin-proof/initramfs.cpio \
 >/proof/origin-proof/serial.log 2>&1
origin_status=$?
set -e
printf '%s\n' "$origin_status" >/proof/origin-proof/status
cat /proof/origin-proof/serial.log
python3 - "$origin_status" <<'ORIGIN_CHECK'
import sys,pathlib,hashlib,json
out=pathlib.Path('/proof/origin-proof');text=(out/'serial.log').read_text(errors='replace');status=int(sys.argv[1])
assert status==33,f'Actual permission QEMU exit {status}, expected33'
for line in (out/'expect').read_text().splitlines()+['kk: 1 CPUs online','kk: process node-memory-proof exited with code 0','KE3 account-origin: ok: actual account identity, foreign memory refusal, charged contiguous DMA and private export']:
 assert line and line in text,f'Missing actual native permission line: {line}'
assert not any(x in text.lower() for x in ['fail:','panic','kk: boot refused','kk: accounting broken'])
records=[]
for p in sorted(out.iterdir()):
 if not p.is_file():continue
 with p.open('rb') as f:digest=hashlib.file_digest(f,'sha256').hexdigest()
 records.append({'name':p.name,'bytes':p.stat().st_size,'sha256':digest})
(out/'origin-artifacts.json').write_text(json.dumps({'passed':True,'scope':'actual registered Account READ|TRANSFER lacksDUP denial on unchanged object-agnostic kernel gate; no Console-specific claim','qemu_exit':status,'files':records},indent=2)+'\n')
print('MORUNA_NATIVE_ORIGIN_BOOT_OK')
ORIGIN_CHECK
"""

class NativeLinkRun(native.NativeRun):
    marker='MORUNA_NATIVE_LINK_DONE'
    measurement_scope='Actual complete selected kernel/native-user graph compilation and static linking, with retained ELF artifacts. Boot, live storage/network behavior and full application acceptance remain separate.'
    runner_files=native.NativeRun.runner_files+('native-link.py',)
    def __init__(self,options,inputs,archive,cache):
        super().__init__(options,inputs,archive,cache);self.guest_script=GUEST.replace('__RUST_SRC_LOCK_SHA256__',inputs['stdlib_provenance']['actual_rust_src_lock_sha256'])
        self.origin_files=[];self.origin_custody=None
        if getattr(options,'origin_proof',None):
            summary_path=module.private_file(options.origin_proof);summary=json.loads(summary_path.read_text())
            require(summary_path.name=='summary.json' and summary_path.parent.name==summary['name'] and summary.get('cleanup') is True and summary.get('stage_passed') is True and summary.get('phase')=='image-tools' and summary.get('guest_exit')==0,'actual retained image-tool cleanup/build missing')
            manifest_path=module.private_file(summary_path.parent/'image-tools/artifacts.json');manifest=json.loads(manifest_path.read_text())
            require(manifest['target']=='aarch64-unknown-linux-gnu' and manifest['toolchain']=='1.98.1' and manifest['profile']=='dev-debug0','actual ARM image-tool input differs')
            records=[r for r in manifest['files'] if r['name']=='moruna-image'];require(len(records)==1,'one actual image writer required')
            record=records[0];tool=module.private_file(manifest_path.parent/'moruna-image');info=tool.stat()
            require(info.st_size==record['bytes'] and info.st_mode&0o111 and hashlib.sha256(tool.read_bytes()).hexdigest()==record['sha256'],'retained image writer actual bytes differ')
            with tool.open('rb') as file:header=file.read(20)
            require(header[:6]==b'\x7fELF\x02\x01' and header[18:20]==b'\xb7\x00','image writer is not actual ARM ELF')
            self.origin_tool=tool
            self.origin_files=[{'path':str(p),'device':p.stat().st_dev,'inode':p.stat().st_ino,'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in [summary_path,manifest_path,tool]]
            self.origin_custody={'summary':str(summary_path),'summary_sha256':self.origin_files[0]['sha256'],'manifest_sha256':self.origin_files[1]['sha256'],'tool':record}
            origin=ORIGIN_GUEST.replace('__IMAGE_TOOL_SHA__',record['sha256'])
            marker="printf 'MORUNA_NATIVE_LINK_DONE"
            require(self.guest_script.count(marker)==1,'native completion seam differs')
            at=self.guest_script.index(marker);self.guest_script=self.guest_script[:at]+origin+self.guest_script[at:]
            self.measurement_scope+=' Optional registered node-memory-proof actual TCG boot and missing-DUP denial; separate artifacts, not a Console-specific test or production KVM proof.'

    def owned(self,cleanup=False):
        if not cleanup:
            for item in self.origin_files:
                p=module.private_file(item['path']);info=p.stat()
                require((info.st_dev,info.st_ino,info.st_size)==(item['device'],item['inode'],item['bytes']) and hashlib.sha256(p.read_bytes()).hexdigest()==item['sha256'],'retained image-tool custody changed')
        return super().owned(cleanup=cleanup)

    def call(self,arguments,*args,**kwargs):
        is_guest=len(arguments)>4 and arguments[:4]==['exec',self.name,'/bin/bash','-c'] and arguments[4]==self.guest_script
        if is_guest and self.origin_custody:
            require(self.owned() is not None,'owned VM missing before image-tool copy')
            super().call(['exec',self.name,'/bin/mkdir','-p','/proof/origin-proof'])
            super().call(['copy',str(self.origin_tool),self.name+':/proof/origin-proof/moruna-image'],seconds=30)
            (self.evidence/'origin-image-tool.json').write_text(json.dumps(self.origin_custody,indent=2)+'\n')
        result=super().call(arguments,*args,**kwargs)
        if len(arguments)>4 and arguments[:4]==['exec',self.name,'/bin/bash','-c'] and arguments[4]==self.guest_script:
            require(self.owned() is not None,'owned native VM missing before artifact collection')
            super().call(['copy',self.name+':/proof/native-link',str(self.evidence/'native-link')],seconds=60,check=False)
            if self.origin_custody:
                require(self.owned() is not None,'owned VM missing before permission-proof retention')
                super().call(['copy',self.name+':/proof/origin-proof',str(self.evidence/'origin-proof')],seconds=45,check=True)
        return result
