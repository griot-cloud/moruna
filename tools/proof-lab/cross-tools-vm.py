#!/usr/bin/env python3
"""Explicit offline CrossToolsRun; no VM action without --run, no cache attachment."""
import argparse,datetime,hashlib,io,json,os,re,signal,stat,sys,tarfile
from pathlib import Path
from attachments import private_directory,flags,validate_inspect
from run import ROOT,Run,LABEL,require,sha
SCRIPTS=('cross-tools-vm.py','cross-tools-linux-smoke.py','run.py','attachments.py','proof-tools.py')
CROSS_SHA='f24b0cfa8edae9aa75f312595088fd44183a2417afed75c9bc8ac3051098f94f'
GUEST=r'''import hashlib,json,os,pathlib,subprocess,sys,tarfile
if not __debug__:raise RuntimeError('optimized Python disables strict helper assertions')
archive,digest,token,qemu,required,cross_sha,rust_sha=sys.argv[1:]
p=pathlib.Path(archive)
assert hashlib.file_digest(p.open('rb'),'sha256').hexdigest()==digest,'source archive hash differs'
pathlib.Path('/proof').mkdir(mode=0o700,exist_ok=True)
src=pathlib.Path('/proof/smoke-source');src.mkdir(mode=0o700)
allowed={'cross-tools-vm.py','cross-tools-linux-smoke.py','run.py','attachments.py','proof-tools.py','rust-bundle.json','rust-manifest.json'}
seen=set()
with tarfile.open(p) as tar:
 for m in tar:
  assert m.name in allowed and m.name not in seen and m.isfile() and m.size<1024*1024,'unsafe helper archive'
  seen.add(m.name)
  with (src/m.name).open('xb') as out:out.write(tar.extractfile(m).read())
assert seen==allowed,'helper census differs'
out=pathlib.Path('/proof/smoke-output');out.mkdir(mode=0o700)
cmd=['python3',str(src/'cross-tools-linux-smoke.py'),'--run-smoke','--bundle','/inputs/cross/bundle.json','--bundle-sha256',cross_sha,'--manifest','/inputs/cross/manifest.json','--archive','/inputs/cross/cross-tools.tar.gz','--rust-bundle',str(src/'rust-bundle.json'),'--rust-bundle-sha256',rust_sha,'--rust-manifest',str(src/'rust-manifest.json'),'--rust-root','/inputs/musl','--output-parent',str(out)]
if qemu:cmd+=['--qemu-x86_64',qemu]
if required=='yes':cmd+=['--require-emulation']
code=subprocess.call(cmd)
# Only explicitly bounded logs/artifacts, never the extracted tools/sysroot.
reports=list(out.glob('cross-smoke-*/summary.json'))
assert len(reports)==1,'missing actual smoke summary'
work=reports[0].parent
names={'summary.json','smoke.c','gnu-smoke','musl-smoke','relocated-musl-gcc.specs'}
kept=[];total=0
with tarfile.open('/proof/smoke-results.tar','w') as tar:
 for f in sorted(work.iterdir()):
  if f.name not in names and not (f.name.endswith('.log') and f.name.replace('-','').replace('.','').isalnum()):continue
  assert f.is_file() and not f.is_symlink() and f.stat().st_size<=4*1024**2,'unsafe smoke output'
  total+=f.stat().st_size;assert total<=32*1024**2,'output aggregate bound'
  tar.add(f,arcname=f.name,recursive=False);kept.append({'path':f.name,'bytes':f.stat().st_size,'mode':f.stat().st_mode&0o777,'sha256':hashlib.sha256(f.read_bytes()).hexdigest()})
pathlib.Path('/proof/smoke-results.json').write_text(json.dumps({'files':kept,'bytes':total,'helper_exit':code},indent=2)+'\n')
print('MORUNA_CROSS_TOOLS_COLLECTED '+token,flush=True)
if code==0:print('MORUNA_CROSS_TOOLS_DONE '+token,flush=True)
sys.exit(code)
'''

def private_file(p):
    p=Path(p).absolute();require(p.is_relative_to(ROOT),'input outside owned proof root')
    private_directory(p.parent);z=p.lstat()
    require(stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1 and not p.is_symlink(),'input not owned real single-link file')
    return p

def load_inputs(cross_raw,rust_raw):
    cross_path=private_file(cross_raw);rust_path=private_file(rust_raw)
    cross=json.loads(cross_path.read_text());rust=json.loads(rust_path.read_text())
    archive=private_file(cross_path.parent/'cross-tools.tar.gz');manifest=private_file(cross_path.parent/'manifest.json')
    require(cross_path.name=='bundle.json' and set(p.name for p in cross_path.parent.iterdir())=={'bundle.json','manifest.json','cross-tools.tar.gz'},'cross mount census differs')
    require(cross['archive_sha256']==CROSS_SHA and sha(archive)==CROSS_SHA and archive.stat().st_size==cross['archive_bytes'],'cross archive differs')
    require(sha(manifest)==cross['manifest_sha256'],'cross manifest differs')
    require(cross.get('archive_readback_verified') is True,'cross virtual verification absent')
    rustroot=private_directory(Path(rust['root']));rm=private_file(rust['manifest'])
    require(rm.parent==rustroot.parent and sha(rm)==rust['manifest_sha256'],'Rust manifest differs')
    z=rustroot.stat();require((z.st_dev,z.st_ino,z.st_uid)==(rust['device'],rust['inode'],rust['uid']),'Rust root identity differs')
    expected={e['path']:e for e in json.loads(rm.read_text())};actual=set();total=0
    require(len(expected)==rust['files']<=100000,'Rust census duplicate/count')
    for base,dirs,files in os.walk(rustroot,followlinks=False):
        for n in dirs:require(not (Path(base)/n).is_symlink(),'Rust directory symlink')
        for n in files:
            p=Path(base)/n;rel=p.relative_to(rustroot).as_posix();z=p.lstat();e=expected.get(rel)
            require(e is not None and stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1 and e['type']=='file' and z.st_size==e['bytes'] and sha(p)==e['sha256'],'Rust input bytes/census differ')
            total+=z.st_size;actual.add(rel)
    require(actual==set(expected) and total==rust['bytes']<=512*1024**2,'Rust totals differ')
    files=[cross_path,archive,manifest,rust_path,rm,*[private_file(ROOT/n) for n in SCRIPTS]]
    fingerprints=[{'path':str(p),'sha256':sha(p),'device':p.stat().st_dev,'inode':p.stat().st_ino,'bytes':p.stat().st_size,'mtime_ns':p.stat().st_mtime_ns} for p in files]
    return {'cross':cross,'cross_path':cross_path,'rust':rust,'rust_path':rust_path,'rust_manifest':rm,'rust_root':rustroot,'fingerprints':fingerprints}

class CrossToolsRun(Run):
    def __init__(self,options,inputs):
        super().__init__(options);self.memory=4;self.inputs=inputs
        self.attachments=[{'path':str(inputs['cross_path'].parent),'destination':'/inputs/cross','readonly':True},{'path':str(inputs['rust_root']),'destination':'/inputs/musl','readonly':True}]
    def verify_unchanged(self):
        for e in self.inputs['fingerprints']:
            p=private_file(e['path']);z=p.stat()
            require((z.st_dev,z.st_ino,z.st_size,z.st_mtime_ns)==(e['device'],e['inode'],e['bytes'],e['mtime_ns']) and sha(p)==e['sha256'],'proof input changed')
    def owned(self,cleanup=False):
        record=super().owned(cleanup=cleanup)
        if record is not None:
            validate_inspect(record['configuration'],self.attachments);c=record['configuration']
            require(c['image']['descriptor']['digest']==self.pins['index'] and c['platform']=={'os':'linux','architecture':'arm64'} and c['resources']['memoryInBytes']==4*1024**3,'cross VM image/platform/resources differ')
        return record
    def helper_archive(self):
        path=self.evidence/'helper-inputs.tar';records=[]
        items={n:ROOT/n for n in SCRIPTS};items.update({'rust-bundle.json':self.inputs['rust_path'],'rust-manifest.json':self.inputs['rust_manifest']})
        with tarfile.open(path,'w') as tar:
            for name,p in sorted(items.items()):
                data=p.read_bytes();require(len(data)<1024*1024,'helper input bound');m=tarfile.TarInfo(name);m.size=len(data);m.mode=0o400;tar.addfile(m,io.BytesIO(data));records.append({'path':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
        path.chmod(0o400);(self.evidence/'helper-inputs-manifest.json').write_text(json.dumps(records,indent=2)+'\n')
        return path
    def collect(self,summary):
        self.owned();code,_=self.call(['copy',self.name+':/proof/smoke-results.tar',str(self.evidence/'smoke-results.tar')],seconds=30,check=False)
        require(code==0,'actual smoke output archive unavailable')
        self.call(['copy',self.name+':/proof/smoke-results.json',str(self.evidence/'smoke-results.json')],seconds=30)
        manifest=json.loads((self.evidence/'smoke-results.json').read_text());expected={e['path']:e for e in manifest['files']};require(len(expected)<=32 and manifest['bytes']<=32*1024**2,'result census bound')
        dest=self.evidence/'outputs';dest.mkdir(mode=0o700);seen=set()
        with tarfile.open(self.evidence/'smoke-results.tar') as tar:
            for m in tar:
                require(m.name in expected and m.name not in seen and '/' not in m.name and m.name not in ('.','..') and m.isfile(),'unsafe result member')
                e=expected[m.name];require(m.size==e['bytes'] and m.size<=4*1024**2,'output size differs');data=tar.extractfile(m).read();require(hashlib.sha256(data).hexdigest()==e['sha256'],'output bytes differ')
                (dest/m.name).write_bytes(data);os.chmod(dest/m.name,e['mode']);seen.add(m.name)
        require(seen==set(expected) and 'summary.json' in seen,'missing result bytes')
        summary['smoke']=json.loads((dest/'summary.json').read_text());summary['outputs_verified']=True
    def execute(self):
        self.evidence.mkdir(mode=0o700);summary={'name':self.name,'nonce':self.token,'mode':self.options.mode,'cpus':2,'memory_gib':4,'pins':self.pins,'cleanup':False,'inputs':self.inputs['fingerprints'],'readonly_mounts':self.attachments,'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat()};failure=None;started=False
        try:
            self.verify_unchanged();archive=self.helper_archive();digest=sha(archive);summary['helper_archive_sha256']=digest
            self.cached_prerequisites()
            self.call(['create','--name',self.name,'--label',LABEL+'='+self.token,'--cpus','2','--memory','4G','--platform','linux/arm64','--init-image',self.pins['init'],'--kernel',self.pins['kernel_path'],'--no-dns',*flags(self.attachments),'--entrypoint','/bin/bash',self.pins['image'],*self.arguments],seconds=45)
            self.owned();self.call(['start',self.name]);started=True;self.owned()
            self.call(['copy',str(archive),self.name+':/tmp/cross-helper-inputs.tar'],seconds=30)
            self.verify_unchanged();self.owned()
            code,text=self.call(['exec',self.name,'python3','-c',GUEST,'/tmp/cross-helper-inputs.tar',digest,self.token,self.options.qemu_x86_64 or '','yes' if self.options.require_emulation else 'no',sha(self.inputs['cross_path']),sha(self.inputs['rust_path'])],seconds=min(self.options.seconds,180),check=False)
            summary['guest_exit']=code
            self.collect(summary)
            require(code==0 and 'MORUNA_CROSS_TOOLS_DONE '+self.token in text and summary['smoke']['passed'] is True,'actual cross smoke failed')
            summary['compile_passed']=True;summary['emulation']=summary['smoke']['emulation'];self.verify_unchanged()
        except BaseException as e:
            failure=e;summary['error']=str(e)
            if started and not summary.get('outputs_verified'):
                try:self.collect(summary)
                except BaseException as collection:summary['collection_error']=str(collection)
        finally:
            signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
            try:self.cleanup();summary['cleanup']=True
            except BaseException as e:summary['cleanup_error']=str(e);failure=e
            summary['finished_utc']=datetime.datetime.now(datetime.timezone.utc).isoformat();(self.evidence/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
            if summary['cleanup'] and self.fingerprint==(self.lock.stat().st_dev,self.lock.stat().st_ino) and (self.lock/'owner').read_text().strip()==self.owner:
                (self.lock/'owner').unlink();self.lock.rmdir()
            print(json.dumps(summary),flush=True)
        return 1 if failure else 0

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--run',action='store_true');p.add_argument('--cross-bundle',default=str(ROOT/'cross-tools-archive-1u_y3fit/bundle.json'));p.add_argument('--rust-bundle',default=str(ROOT/'rust-musl-component/musl-target-vgegt4ot/bundle.json'));p.add_argument('--seconds',type=int,default=360);p.add_argument('--qemu-x86_64');p.add_argument('--require-emulation',action='store_true');o=p.parse_args();o.mode='cross-tools-smoke'
    require(180<=o.seconds<=600,'bounded stage must be180..600seconds')
    require(not o.require_emulation or o.qemu_x86_64,'required emulation needs explicit emulator')
    require(not o.qemu_x86_64 or (o.qemu_x86_64.startswith('/') and re.fullmatch(r'/[A-Za-z0-9_./-]+',o.qemu_x86_64)),'explicit Linux emulator path required')
    inputs=load_inputs(o.cross_bundle,o.rust_bundle)
    if not o.run:print(json.dumps({'prepared_only':True,'no_container_commands':True,'cross_archive_sha256':inputs['cross']['archive_sha256'],'rust_manifest_sha256':inputs['rust']['manifest_sha256']}));return 0
    os.umask(0o077);run=CrossToolsRun(o,inputs)
    def interrupted(signum,frame):raise RuntimeError('interrupted signal '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:run.acquire()
    except RuntimeError as e:print(str(e),file=sys.stderr);return 75
    return run.execute()
if __name__=='__main__':sys.exit(main())
