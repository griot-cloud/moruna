#!/usr/bin/env python3
"""One owned same-release QEMU build. Prepared-only unless --run; no image/package install."""
import argparse,base64,datetime,json,os,shutil,signal,stat,sys,time
from pathlib import Path
from attachments import private_directory,flags,validate_inspect
from run import ROOT,Run,LABEL,require,sha
TAR_SHA='a3717477d8e2c84d630bfffbc20f6cd3293eb45aa1e6dac6d0cc27689991c9e1'
GUEST=r'''set -euo pipefail
token="$1"
printf '%s\n' "$token" >/run/moruna-proof-owner
mkdir -p /proof/qemu-common/install/lib /proof/qemu-deps /proof/qemu-common/pkgconfig
echo '__TAR_SHA__  /inputs/qemu/qemu-10.2.1.tar.xz' | sha256sum -c -
echo '9773f211dcacb21f83f33475af607e05670df4a5bcd6135dbd3bdead1688eeff  /inputs/slirp/libslirp-dev_4.7.0-1_arm64.deb' | sha256sum -c -
echo '54baa3272a0aad6413186a7e6ab2762b8181f3573e65aa82eecbec9a55a9dfe2  /inputs/slirp/libslirp0_4.7.0-1_arm64.deb' | sha256sum -c -
# --extract only: NO package database/install operation or base filesystem write.
dpkg-deb --extract /inputs/slirp/libslirp-dev_4.7.0-1_arm64.deb /proof/qemu-deps
dpkg-deb --extract /inputs/slirp/libslirp0_4.7.0-1_arm64.deb /proof/qemu-deps
python3 - <<'PY_INPUT'
import base64,pathlib
root=pathlib.Path('/proof/qemu-common')
text=pathlib.Path('/proof/qemu-deps/usr/lib/aarch64-linux-gnu/pkgconfig/slirp.pc').read_text()
assert text.count('prefix=/usr\n')==1,'actual slirp.pc shape changed'
(root/'pkgconfig/slirp.pc').write_text(text.replace('prefix=/usr\n','prefix=/proof/qemu-deps/usr\n'))
(root/'qemu-slirp.sh').write_bytes(base64.b64decode('__RECIPE__'))
PY_INPUT
cp /proof/qemu-deps/usr/lib/aarch64-linux-gnu/libslirp.so.0.4.0 /proof/qemu-common/install/lib/
ln -s libslirp.so.0.4.0 /proof/qemu-common/install/lib/libslirp.so.0
ln -s libslirp.so.0 /proof/qemu-common/install/lib/libslirp.so
export PKG_CONFIG_PATH=/proof/qemu-common/pkgconfig
export LD_LIBRARY_PATH=/proof/qemu-common/install/lib
[[ "$(pkg-config --modversion slirp)" = 4.7.0 ]]
bash /proof/qemu-common/qemu-slirp.sh "$token" /inputs/qemu/qemu-10.2.1.tar.xz
readelf -hW /proof/qemu-common/install/bin/qemu-system-x86_64 | tee /proof/qemu-common/install/elf-header.txt
grep -q AArch64 /proof/qemu-common/install/elf-header.txt
readelf -dW /proof/qemu-common/install/bin/qemu-system-x86_64 | tee /proof/qemu-common/install/elf-dynamic.txt
grep -q 'libslirp.so.0' /proof/qemu-common/install/elf-dynamic.txt
grep -q '/proof/qemu-common/install/lib' /proof/qemu-common/install/elf-dynamic.txt
cp /proof/qemu-common/{version.txt,netdev.txt,devices.txt,installed.sha256} /proof/qemu-common/install/
python3 - <<'PY_MANIFEST'
import hashlib,json,pathlib
root=pathlib.Path('/proof/qemu-common/install');rows=[];total=0
for p in sorted(root.rglob('*')):
 if p.is_symlink():
  assert p.resolve(strict=True).is_relative_to(root),'output symlink escape'
  rows.append({'path':str(p.relative_to(root)),'type':'symlink','target':str(p.readlink())})
 elif p.is_dir():continue
 else:
  assert p.is_file() and p.stat().st_nlink==1,'output not regular'
  total+=p.stat().st_size;assert total<=1024**3 and len(rows)<50000,'output bound'
  with p.open('rb') as f:d=hashlib.file_digest(f,'sha256').hexdigest()
  rows.append({'path':str(p.relative_to(root)),'type':'file','bytes':p.stat().st_size,'sha256':d,'mode':p.stat().st_mode&0o777})
(root/'.proof-output.json').write_text(json.dumps({'version':1,'target':'x86_64-softmmu','host':'aarch64-linux','source_sha256':'__TAR_SHA__','prefix':str(root),'files':rows,'bytes':total},indent=2)+'\n')
PY_MANIFEST
printf 'MORUNA_QEMU_VSOCK_READY %s\n' "$token"
'''

def inputs(raw):
    p=Path(raw).resolve(strict=True);require(p.is_relative_to(ROOT),'input record outside proof root');d=json.loads(p.read_text())
    for item in d['files']:
        f=Path(item['path']);private_directory(f.parent);z=f.lstat()
        require(stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1 and z.st_size==item['bytes'] and sha(f)==item['sha256'],'input custody differs')
    require(d['source_sha256']==TAR_SHA,'QEMU source pin differs')
    return d

class QemuBuild(Run):
    def __init__(self,o,d):
        super().__init__(o);self.memory=6;self.input_record=d
        self.output=ROOT/('qemu-vsock-install-'+self.token);self.output.mkdir(mode=0o700)
        info=self.output.stat();self.output_identity=(info.st_dev,info.st_ino)
        self.attachments=[{'path':d['source_directory'],'destination':'/inputs/qemu','readonly':True},
                          {'path':d['slirp_directory'],'destination':'/inputs/slirp','readonly':True},
                          {'path':str(self.output),'destination':'/proof/qemu-common/install','readonly':False}]
    def owned(self,cleanup=False):
        d=inputs(self.options.inputs);require(d==self.input_record,'input record changed')
        require((self.output.stat().st_dev,self.output.stat().st_ino)==self.output_identity and not self.output.is_symlink(),'owned output changed')
        rec=super().owned(cleanup=cleanup)
        if rec is not None:
            c=rec['configuration'];validate_inspect(c,self.attachments)
            require(c['image']['descriptor']['digest']==self.pins['index'] and c['platform']=={'os':'linux','architecture':'arm64'} and c['resources']['memoryInBytes']==6*1024**3,'VM image/platform/resources differ')
        return rec
    def execute(self):
        self.evidence.mkdir(mode=0o700);failed=False
        summary={'name':self.name,'nonce':self.token,'mode':self.options.mode,'cleanup':False,'passed':False,'output':str(self.output),'output_identity':self.output_identity,'inputs':self.input_record,'scripts':{n:sha(ROOT/n) for n in ('qemu-vsock-build.py','qemu-slirp.sh','run.py','attachments.py')},'scope':'owned same-release QEMU build/device listing; NOT guest boot or workload acceptance'}
        try:
            self.cached_prerequisites()
            self.call(['create','--name',self.name,'--label',LABEL+'='+self.token,'--cpus','2','--memory','6G','--platform','linux/arm64','--init-image',self.pins['init'],'--kernel',self.pins['kernel_path'],'--no-dns',*flags(self.attachments),'--entrypoint','/bin/bash',self.pins['image'],*self.arguments],seconds=45)
            require(self.owned() is not None,'owned VM missing');self.call(['start',self.name]);require(self.owned() is not None,'owned VM missing')
            recipe=Path(self.input_record['recipe']).read_bytes()
            guest=GUEST.replace('__TAR_SHA__',TAR_SHA).replace('__RECIPE__',base64.b64encode(recipe).decode())
            code,text=self.call(['exec',self.name,'/bin/bash','-c',guest,'--',self.token],seconds=max(1,int(self.deadline-time.monotonic())-45),check=False)
            summary['guest_exit']=code;require(code==0 and 'MORUNA_QEMU_VSOCK_READY '+self.token in text,'actual QEMU build/device checks failed')
            summary['passed']=True
        except BaseException as error:summary['error']=type(error).__name__+': '+str(error);failed=True
        finally:
            signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
            try:self.cleanup();summary['cleanup']=True
            except BaseException as error:summary['cleanup_error']=str(error);failed=True
            try:
                record_path=self.output/'.proof-output.json'
                if summary['passed']:
                    require(summary['cleanup'],'VM not cleaned before output readback');record=json.loads(record_path.read_text());actual=set()
                    for p in self.output.rglob('*'):
                        if p.is_dir() and not p.is_symlink():continue
                        if p.name=='.proof-output.json':continue
                        actual.add(str(p.relative_to(self.output)))
                    require(actual=={r['path'] for r in record['files']},'output census mismatch')
                    for r in record['files']:
                        p=self.output/r['path'];require(p.resolve(strict=True).is_relative_to(self.output),'output escape')
                        if r['type']=='file':require(not p.is_symlink() and p.stat().st_size==r['bytes'] and sha(p)==r['sha256'],'actual output bytes differ')
                        else:require(p.is_symlink() and str(p.readlink())==r['target'],'output link differs')
                    summary['manifest_sha256']=sha(record_path);summary['artifacts']=record
            except BaseException as error:summary['output_error']=str(error);failed=True
            summary['passed']=summary['passed'] and summary['cleanup'] and not failed
            (self.evidence/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
            if summary['cleanup'] and self.fingerprint==(self.lock.stat().st_dev,self.lock.stat().st_ino) and (self.lock/'owner').read_text().strip()==self.owner:
                (self.lock/'owner').unlink();self.lock.rmdir()
            print(json.dumps({'summary':str(self.evidence/'summary.json'),'passed':summary['passed'],'output':str(self.output)}),flush=True)
        return 0 if summary['passed'] else 1

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--inputs',required=True);p.add_argument('--run',action='store_true');p.add_argument('--seconds',type=int,default=1100);o=p.parse_args();o.mode='qemu-vsock-build'
    require(60<=o.seconds<=1100,'build bound must be60..1100s');d=inputs(o.inputs)
    if not o.run:print(json.dumps({'prepared_only':True,'inputs':o.inputs,'source_sha256':TAR_SHA}));return 0
    run=QemuBuild(o,d)
    def interrupted(signum,frame):raise RuntimeError('interrupted '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:run.acquire()
    except RuntimeError as error:print(error,file=sys.stderr);return 75
    return run.execute()
if __name__=='__main__':sys.exit(main())
