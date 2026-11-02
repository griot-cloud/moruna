#!/usr/bin/env python3
"""Bounded owned provisioning; no host install, foreign builder or default mutation."""
import argparse,datetime,hashlib,json,os,secrets,signal,stat,sys,time
from pathlib import Path
from attachments import flags,private_directory,validate_inspect
from run import ROOT,LABEL,Run,require,sha

PROFILES={'std':['clang','libclang-dev'],
          'kernel':['clang','libclang-dev','gcc-x86-64-linux-gnu','musl-dev:amd64','libslirp-dev','dwarves','libelf-dev','bc']}

def cache_new(profile):
    nonce=secrets.token_hex(16)
    parent=ROOT/'package-cache';parent.mkdir(mode=0o700,exist_ok=True);private_directory(parent)
    root=parent/nonce;root.mkdir(mode=0o700)
    for name in ['debs','lists']:(root/name).mkdir(mode=0o700)
    info=root.stat()
    value={'owner_nonce':nonce,'path':str(root),'device':info.st_dev,'inode':info.st_ino,
           'base_index':json.loads((ROOT/'pins.json').read_text())['index'],'profile':profile,
           'packages':PROFILES[profile],'phase':'download-only; nothing installed',
           'journal_path':str(root/'journal.json')}
    (root/'journal.json').write_text(json.dumps(value,indent=2)+'\n')
    return value

def cache_validate(value):
    root=private_directory(Path(value['path']))
    require(root.name==value['owner_nonce'] and (root.stat().st_dev,root.stat().st_ino)==(value['device'],value['inode']),'package cache identity differs')
    journal=root/'journal.json';info=journal.lstat()
    require(info.st_uid==os.getuid() and info.st_nlink==1 and not journal.is_symlink(),'package journal identity differs')
    require(json.loads(journal.read_text())==value,'package journal changed')
    require(value['base_index']==json.loads((ROOT/'pins.json').read_text())['index'],'package base differs')
    require(value['packages']==PROFILES[value['profile']],'package profile differs')
    total=0
    for p in root.rglob('*'):
        i=p.lstat();require(not stat.S_ISLNK(i.st_mode) and i.st_uid==os.getuid(),'package path ownership/type differs')
        if p.is_file():total+=i.st_size
    require(total<=2*1024**3,'package cache exceeds2GiB')
    return root

class Provision(Run):
    def __init__(self,options,cache):
        super().__init__(options);self.name=f'moruna-env-{os.getuid()}-{self.token[:12]}'
        self.evidence=ROOT/self.name;self.cache=cache;self.cache_root=cache_validate(cache);self.memory=4
        self.attachments=[{'path':str(self.cache_root/'debs'),'destination':'/inputs/debs','readonly':False},
                          {'path':str(self.cache_root/'lists'),'destination':'/var/lib/apt/lists','readonly':False}]

    def execute(self):
        self.evidence.mkdir(mode=0o700)
        summary={'name':self.name,'nonce':self.token,'phase':self.options.phase,'profile':self.options.profile,
                 'cache_journal':self.cache['journal_path'],'base':self.pins,'cleanup':False,
                 'scope':'Owned dependency preparation only; no source/compiler/kernel capability claim',
                 'scripts':{n:sha(ROOT/n) for n in ['provision-env.py','resolve-tools.sh','install-locked-debs.py','run.py','attachments.py']}}
        failure=None
        try:
            self.cached_prerequisites();cache_validate(self.cache)
            self.call(['create','--name',self.name,'--label',LABEL+'='+self.token,'--cpus','2','--memory','4G',
                       '--platform','linux/arm64','--init-image',self.pins['init'],'--kernel',self.pins['kernel_path'],
                       *flags(self.attachments),'--entrypoint','/bin/bash',self.pins['image'],*self.arguments],seconds=45)
            config=self.owned()['configuration'];validate_inspect(config,self.attachments)
            require(config['image']['descriptor']['digest']==self.pins['index'],'provision base image changed')
            require(config['platform']['architecture']=='arm64','provision architecture differs')
            self.call(['start',self.name]);self.owned()
            self.call(['exec',self.name,'/bin/mkdir','-p','/proof-tools','/proof'])
            for name in ['resolve-tools.sh','install-locked-debs.py']:
                self.owned();self.call(['copy',str(ROOT/name),self.name+':/proof-tools/'+name])
            self.call(['exec',self.name,'/bin/bash','-c','printf "%s\\n" "$1" > /run/moruna-proof-owner','--',self.token])
            if self.options.phase=='resolve':
                # Guest timeout ends its own process group before host evidence copy;
                # apt partial archives/lists survive only in the exact owned mounts.
                budget=max(30,self.options.seconds-75)
                code,out=self.call(['exec',self.name,'/usr/bin/timeout','--kill-after=5',str(budget),'/bin/bash',
                                    '/proof-tools/resolve-tools.sh',self.token,*self.cache['packages']],seconds=budget+10,check=False)
                summary['guest_exit']=code
                self.owned();self.call(['copy',self.name+':/proof/package-resolution',str(self.evidence/'package-resolution')],seconds=30,check=False)
                require(code==0 and 'MORUNA_PACKAGE_LOCK_READY_FOR_REVIEW '+self.token in out,'package resolution incomplete; exact owned downloads retained')
                lock=self.evidence/'package-resolution/package-lock.json'
                require(lock.is_file() and not lock.is_symlink(),'resolved package lock missing')
                packages=json.loads(lock.read_text())['packages'];require(packages,'empty resolved package closure')
                for p in packages:
                    name=Path(p['path']);require(name.parent==Path('/inputs/debs') and name.suffix=='.deb','package lock path differs')
                    archive=self.cache_root/'debs'/name.name
                    require(archive.is_file() and not archive.is_symlink() and sha(archive)==p['sha256'] and archive.stat().st_size==p['bytes'],'resolved package archive checksum differs')
                summary.update(stage_passed=True,package_lock=str(lock),package_lock_sha256=sha(lock),packages=len(packages))
            else:
                lock=Path(self.options.lock).resolve(strict=True)
                require(lock.is_relative_to(ROOT) and sha(lock)==self.options.lock_sha256,'reviewed package lock differs')
                packages=json.loads(lock.read_text())['packages'];require(packages,'empty install lock')
                for p in packages:
                    name=Path(p['path']);require(name.parent==Path('/inputs/debs'),'unreviewed archive path')
                    archive=self.cache_root/'debs'/name.name;require(sha(archive)==p['sha256'],'offline archive differs')
                self.owned();self.call(['copy',str(lock),self.name+':/proof/package-lock.json'])
                if any(p['architecture']=='amd64' for p in packages):self.call(['exec',self.name,'dpkg','--add-architecture','amd64'])
                code,out=self.call(['exec',self.name,'python3','/proof-tools/install-locked-debs.py',self.token,'/proof/package-lock.json'],seconds=max(30,self.options.seconds-100),check=False)
                summary['guest_exit']=code;require(code==0 and 'MORUNA_LOCKED_PACKAGES_INSTALLED '+self.token in out,'offline installation failed')
                self.call(['copy',self.name+':/proof/installed-packages.txt',str(self.evidence/'installed-packages.txt')])
                smoke='''set -e
clang --version
python3 - <<'PY'
import ctypes,glob
loaded=[]
for name in sorted(set(glob.glob('/usr/lib/llvm-*/lib/libclang.so*')+glob.glob('/usr/lib/aarch64-linux-gnu/libclang*.so*'))):
    try:
        library=ctypes.CDLL(name)
        getattr(library,'clang_getClangVersion')
        loaded.append(name)
    except (OSError,AttributeError):
        continue
if not loaded:raise SystemExit('No actual libclang C API shared library loads')
print('MORUNA_LIBCLANG_LOAD_OK',loaded)
PY'''
                _,out=self.call(['exec',self.name,'/bin/bash','-c',smoke],seconds=30)
                require('MORUNA_LIBCLANG_LOAD_OK' in out,'actual libclang load marker missing')
                inventory='''import hashlib,json,pathlib,subprocess
toolroot=pathlib.Path(subprocess.check_output(['rustup','run','nightly-2026-07-21','rustc','--print','sysroot'],text=True).strip())
library=toolroot/'lib/rustlib/src/rust/library'
if not (library/'core/Cargo.toml').is_file():raise SystemExit('Actual cached nightly rust-src core missing')
paths=[library/'Cargo.toml',library/'Cargo.lock',library.parent/'Cargo.lock',library/'compiler-builtins/Cargo.toml',library/'compiler_builtins/Cargo.toml']
result={'sysroot':str(toolroot),'actual_source_files':[]}
for path in paths:
    if path.is_file():
        data=path.read_bytes();result['actual_source_files'].append({'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
        if path.name=='Cargo.lock':pathlib.Path('/proof/stdlib-'+str(len(result['actual_source_files']))+'.lock').write_bytes(data)
pathlib.Path('/proof/rust-src-inventory.json').write_text(json.dumps(result,indent=2)+'\\n')
print('MORUNA_CACHED_RUST_SRC_RECORDED')'''
                self.call(['exec',self.name,'python3','-c',inventory],seconds=30)
                self.call(['copy',self.name+':/proof/rust-src-inventory.json',str(self.evidence/'rust-src-inventory.json')])
                # Commit is the official CLI operation, never shared buildkit. Its
                # full-root export needs a separately enforced space reserve.
                import shutil
                require(shutil.disk_usage(ROOT).free>=12*1024**3,'full-root commit needs12GiB free; install source/packages remain reproducible')
                image='moruna-proof-env-'+self.token+':1'
                _,images=self.json_call(['image','list','--format','json'])
                require(not any(str(i.get('name','')).endswith('/'+image) or i.get('name')==image for i in images),'new image tag collision')
                self.owned();self.call(['stop','--time','3',self.name],seconds=20)
                self.owned();self.call(['commit',self.name,image],seconds=180)
                _,observed=self.json_call(['image','inspect',image]);require(len(observed)==1,'committed image readback missing')
                (self.evidence/'committed-image.json').write_text(json.dumps(observed,indent=2)+'\n')
                summary.update(stage_passed=True,image=image,image_index=observed[0]['configuration']['descriptor']['digest'],
                               package_lock_sha256=sha(lock),installed_inventory_sha256=sha(self.evidence/'installed-packages.txt'))
        except BaseException as e:
            failure=e;summary['error']=type(e).__name__+': '+str(e)
        finally:
            signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
            try:self.cleanup();summary['cleanup']=True
            except BaseException as e:summary['cleanup_error']=type(e).__name__+': '+str(e);failure=e
            summary['finished_at']=datetime.datetime.now(datetime.timezone.utc).isoformat()
            (self.evidence/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
            if summary['cleanup'] and self.fingerprint==(self.lock.stat().st_dev,self.lock.stat().st_ino) and (self.lock/'owner').read_text().strip()==self.owner:
                (self.lock/'owner').unlink();self.lock.rmdir()
            print(json.dumps(summary),flush=True)
        return 1 if failure else 0

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--run',action='store_true')
    p.add_argument('--phase',choices=['resolve','install'],required=True);p.add_argument('--profile',choices=PROFILES,default='std')
    p.add_argument('--cache-journal');p.add_argument('--lock');p.add_argument('--lock-sha256');p.add_argument('--seconds',type=int,default=180)
    options=p.parse_args();options.mode='provision';require(120<=options.seconds<=600,'bounded provisioning120..600seconds')
    if not options.run:print(json.dumps({'prepared_only':True,'phase':options.phase,'packages':PROFILES[options.profile]}));return 0
    os.umask(0o077)
    cache=json.loads(Path(options.cache_journal).read_text()) if options.cache_journal else cache_new(options.profile)
    require(cache['profile']==options.profile,'cache/profile mismatch');cache_validate(cache)
    if options.phase=='install':require(options.lock and options.lock_sha256,'exact reviewed lock required')
    run=Provision(options,cache)
    def interrupted(signum,frame):raise RuntimeError('interrupted '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:run.acquire()
    except RuntimeError as e:print(str(e),file=sys.stderr);return 75
    return run.execute()

if __name__=='__main__':sys.exit(main())
