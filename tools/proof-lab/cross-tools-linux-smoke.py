#!/usr/bin/env python3
"""Offline Linux-only archive verification, private extraction and real C tool smoke.

No system installation or rustup metadata mutation. All command execution requires
--run-smoke and happens only on Linux inside the separately owned proof VM.
"""
import argparse,hashlib,json,os,posixpath,stat,struct,subprocess,sys,tarfile,tempfile,time
from pathlib import Path,PurePosixPath
MAX_FILES=100000
MAX_BYTES=512*1024**2

def require(ok,msg):
    if not ok:raise RuntimeError(msg)
def sha(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def clean(s):
    require(isinstance(s,str) and s and len(s.encode())<=4096 and '\x00' not in s and '\\' not in s,'invalid path')
    p=PurePosixPath(s)
    require(not p.is_absolute() and all(x not in ('','..','.') for x in s.split('/')),'unsafe path '+s)
    require(str(p)==s,'noncanonical path '+s)
    return s
def manifest(path,digest):
    require(path.stat().st_size<32*1024**2 and sha(path)==digest,'manifest hash/size differs')
    rows=json.loads(path.read_text());require(isinstance(rows,list) and len(rows)<=MAX_FILES,'manifest count bound')
    result={}
    for row in rows:
        name=clean(row['path']);require(name not in result,'duplicate manifest path')
        require(row['type'] in ('file','directory','symlink','hardlink'),'invalid manifest type')
        result[name]=row
    return result
def links(tree):
    def resolve(name,trail=()):
        require(len(trail)<64 and name not in trail,'link cycle')
        parts=name.split('/')
        for i in range(1,len(parts)+1):
            prefix='/'.join(parts[:i]);e=tree.get(prefix);require(e is not None,'missing target '+prefix)
            if e['type'] in ('symlink','hardlink'):
                target=e['resolved_target'];suffix='/'.join(parts[i:])
                return resolve(posixpath.join(target,suffix) if suffix else target,trail+(name,))
            require(i==len(parts) or e['type']=='directory','file parent')
        return tree[name]
    for name,e in tree.items():
        parent=posixpath.dirname(name)
        while parent:
            require(parent in tree and tree[parent]['type']=='directory','symlink/missing parent');parent=posixpath.dirname(parent)
        if e['type'] in ('symlink','hardlink'):
            target=e['target'];require(target and '\x00' not in target and '\\' not in target and not target.startswith('/'),'unsafe link target')
            dest=posixpath.normpath(posixpath.join(posixpath.dirname(name),target) if e['type']=='symlink' else target)
            require(clean(dest)==e['resolved_target'],'link target manifest differs')
            final=resolve(name)
            require(e['type']!='hardlink' or final['type']=='file','hardlink not file')

def extract(archive,tree,root,bundle):
    links(tree);seen=set();total=0;modes={}
    # First pass validates complete archive without touching destination Linux paths.
    with tarfile.open(archive,'r:gz') as tar:
        for m in tar:
            n=clean(m.name);require(n not in seen and n in tree,'archive census differs');seen.add(n)
            require(len(seen)<=MAX_FILES,'archive member bound');e=tree[n]
            require(m.uid==m.gid==0 and m.mode in (0o444,0o555),'unexpected ownership/mode')
            modes[n]=m.mode
            if e['type']=='file':
                require(m.isfile() and 0<=m.size<=MAX_BYTES and m.size==e['bytes'],'file geometry differs')
                total+=m.size;require(total<=MAX_BYTES,'archive byte bound')
                require(hashlib.file_digest(tar.extractfile(m),'sha256').hexdigest()==e['sha256'],'archive file hash differs')
            elif e['type']=='directory':require(m.isdir() and m.size==0,'directory differs')
            else:require((m.issym() if e['type']=='symlink' else m.islnk()) and m.size==0 and m.linkname==e['target'],'link differs')
    require(seen==set(tree) and total==bundle['bytes'] and len(seen)==bundle['entries'],'archive totals differ')
    root.mkdir(mode=0o700)
    # Fail explicitly if the destination cannot preserve Linux case distinctions.
    a=root/'CaseProbe';b=root/'caseprobe'
    a.write_bytes(b'A')
    try:
        with b.open('xb') as f:f.write(b'B')
        require(a.stat().st_ino!=b.stat().st_ino,'case-insensitive destination')
    finally:
        a.unlink()
        if b.exists():b.unlink()
    for n,e in sorted(tree.items(),key=lambda p:(p[0].count('/'),p[0])):
        if e['type']=='directory':(root/n).mkdir(mode=0o700)
    with tarfile.open(archive,'r:gz') as tar:
        for m in tar:
            e=tree[m.name]
            if e['type']!='file':continue
            dest=root/m.name
            fd=os.open(dest,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
            with os.fdopen(fd,'wb') as out,tar.extractfile(m) as src:
                count=0;h=hashlib.sha256()
                while data:=src.read(1024*1024):
                    count+=len(data);require(count<=e['bytes'],'extract overflow');out.write(data);h.update(data)
                require(count==e['bytes'] and h.hexdigest()==e['sha256'],'extract changed bytes')
            dest.chmod(modes[m.name])
    for n,e in tree.items():
        if e['type']=='symlink':(root/n).symlink_to(e['target'])
    unresolved={n:e for n,e in tree.items() if e['type']=='hardlink'}
    while unresolved:
        ready=[n for n,e in unresolved.items() if (root/e['resolved_target']).is_file()]
        require(ready,'unresolved hardlinks')
        for n in ready:
            e=unresolved.pop(n);os.link((root/e['resolved_target']).resolve(strict=True),root/n,follow_symlinks=False)
    for n,e in tree.items():
        p=root/n
        require(p.resolve(strict=True).is_relative_to(root),'extracted link escapes')
        if e['type']=='file':require(sha(p)==e['sha256'] and p.stat().st_size==e['bytes'],'extracted byte mismatch')
    for n,e in sorted(tree.items(),reverse=True):
        if e['type']=='directory':(root/n).chmod(0o555)
    return total

def verify_rust(root,tree,bundle):
    require(bundle['source_archive_sha256']=='bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f','wrong official musl component')
    actual=set();total=0
    for base,dirs,files in os.walk(root,followlinks=False):
        for name in dirs:require(not (Path(base)/name).is_symlink(),'Rust directory symlink')
        for name in files:
            p=Path(base)/name;n=p.relative_to(root).as_posix();require(n in tree and not p.is_symlink(),'Rust census differs')
            e=tree[n];require(e['type']=='file' and p.stat().st_size==e['bytes'] and sha(p)==e['sha256'],'Rust bytes differ')
            total+=e['bytes'];actual.add(n);require(total<=MAX_BYTES,'Rust byte bound')
    require(actual==set(tree) and len(actual)==bundle['files'] and total==bundle['bytes'],'Rust totals differ')
    require(any(n.startswith('lib/libstd-') and n.endswith('.rlib') for n in actual),'actual Rust musl std missing')
    return {'files':len(actual),'bytes':total,'rustup_metadata_modified':False}

def compiler_arguments(root):
    """Exact private ARM-hosted x86 GCC search/sysroot arguments."""
    libgcc=root/'usr/lib/gcc-cross/x86_64-linux-gnu/12';gnu=root/'usr/x86_64-linux-gnu'
    return [root/'usr/bin/x86_64-linux-gnu-gcc','-B'+str(libgcc)+'/', '-B'+str(gnu/'bin')+'/', '--sysroot='+str(root),'-O2']

def relocate_musl_specs(root, output):
    """Derive a private specs file; original authenticated bytes stay immutable."""
    specs=root/'usr/lib/x86_64-linux-musl/musl-gcc.specs';text=specs.read_text()
    require('/usr/include/x86_64-linux-musl' in text and '/usr/lib/x86_64-linux-musl' in text,'musl specs shape changed')
    output.write_text(text.replace('/usr/include/x86_64-linux-musl',str(root/'usr/include/x86_64-linux-musl')).replace('/usr/lib/x86_64-linux-musl',str(root/'usr/lib/x86_64-linux-musl')))
    return output

def main():
    p=argparse.ArgumentParser(description=__doc__)
    for n in ('bundle','manifest','archive','rust-bundle','rust-manifest','rust-root','output-parent'):p.add_argument('--'+n,type=Path,required=True)
    for n in ('bundle-sha256','rust-bundle-sha256'):p.add_argument('--'+n,required=True)
    p.add_argument('--qemu-x86_64',type=Path,help='Explicit existing qemu-x86_64 userspace emulator; absence records NOT RUN')
    p.add_argument('--require-emulation',action='store_true')
    p.add_argument('--run-smoke',action='store_true',help='Required execution acknowledgement, only within owned Linux VM')
    a=p.parse_args();require(sys.platform=='linux' and a.run_smoke,'Linux owned-proof execution with --run-smoke required')
    require(sha(a.bundle)==a.bundle_sha256 and sha(a.rust_bundle)==a.rust_bundle_sha256,'bundle identity differs')
    bundle=json.loads(a.bundle.read_text());rust=json.loads(a.rust_bundle.read_text())
    require(a.archive.stat().st_size==bundle['archive_bytes'] and sha(a.archive)==bundle['archive_sha256'],'archive identity differs')
    require(bundle['archive_sha256']=='f24b0cfa8edae9aa75f312595088fd44183a2417afed75c9bc8ac3051098f94f','unexpected cross tool input')
    tree=manifest(a.manifest,bundle['manifest_sha256']);rusttree=manifest(a.rust_manifest,rust['manifest_sha256'])
    require(a.output_parent.is_absolute() and a.output_parent.is_dir(),'explicit existing output parent required')
    for part in [a.output_parent,*a.output_parent.parents]:require(not part.is_symlink(),'output parent symlink')
    require(a.output_parent.stat().st_uid==os.getuid(),'output parent not owned')
    os.umask(0o077);work=Path(tempfile.mkdtemp(prefix='cross-smoke-',dir=a.output_parent));report={'work':str(work),'passed':False,'commands':[],'scope':'GNU/musl C cross compiler plus exact Rust musl library availability, not a Rust build or installed rustup component claim'}
    try:
        root=work/'tools';report['extracted_bytes']=extract(a.archive,tree,root,bundle)
        report['rust_libraries']=verify_rust(a.rust_root,rusttree,rust)
        env={'PATH':str(root/'usr/bin')+':/usr/bin:/bin','HOME':str(work),'TMPDIR':str(work),'LC_ALL':'C','LANG':'C','LD_LIBRARY_PATH':str(root/'usr/lib/aarch64-linux-gnu')}
        deadline=time.monotonic()+120
        def call(args,label):
            remaining=deadline-time.monotonic();require(remaining>0,'global120s smoke deadline')
            log=work/(label+'.log')
            with log.open('xb') as output:
                import resource
                def bounded_files():resource.setrlimit(resource.RLIMIT_FSIZE,(4*1024**2,4*1024**2))
                process=subprocess.Popen([str(x) for x in args],stdout=output,stderr=subprocess.STDOUT,env=env,start_new_session=True,preexec_fn=bounded_files)
                try:
                    while process.poll() is None:
                        require(time.monotonic()<deadline and log.stat().st_size<4*1024**2,'command time/output bound');time.sleep(.05)
                finally:
                    if process.poll() is None:
                        import signal
                        os.killpg(process.pid,signal.SIGKILL);process.wait()
            require(log.stat().st_size<=4*1024**2,'command output bound')
            report['commands'].append({'argv':[str(x) for x in args],'exit':process.returncode,'log':str(log),'log_sha256':sha(log)})
            require(process.returncode==0,'actual command failed: '+label)
            return log.read_text()
        gcc=root/'usr/bin/x86_64-linux-gnu-gcc';readelf=root/'usr/bin/x86_64-linux-gnu-readelf'
        require(gcc.is_file() and readelf.is_file(),'real cross compiler/readelf missing')
        version=call([gcc,'-dumpfullversion'],'gcc-version').strip();require(version=='12.2.0','unexpected GCC version')
        require(call([gcc,'-dumpmachine'],'gcc-machine').strip()=='x86_64-linux-gnu','wrong compiler target')
        libgcc=root/'usr/lib/gcc-cross/x86_64-linux-gnu/12';gnu=root/'usr/x86_64-linux-gnu'
        common=compiler_arguments(root)
        src=work/'smoke.c';src.write_text('#include <stdio.h>\n#include <stdint.h>\n#if defined(EXPECT_GNU) && !defined(__GLIBC__)\n#error GNU headers missing\n#endif\n#if defined(EXPECT_MUSL) && defined(__GLIBC__)\n#error GNU headers leaked into musl\n#endif\nint main(void) { volatile uint64_t n=40; printf("MORUNA_CROSS_SMOKE_%llu\\n", (unsigned long long)(n+2)); return 0; }\n')
        gnuout=work/'gnu-smoke';muslout=work/'musl-smoke'
        call(common+['-DEXPECT_GNU=1','-B'+str(gnu/'lib')+'/','-nostdinc','-isystem',libgcc/'include','-isystem',gnu/'include',src,'-L'+str(gnu/'lib'),'-o',gnuout],'gnu-compile')
        # Keep original specs immutable. Relocate only its explicit sysroot paths
        # in a separately recorded derived specs file, never use host musl headers.
        specs=root/'usr/lib/x86_64-linux-musl/musl-gcc.specs';text=specs.read_text()
        require('/usr/include/x86_64-linux-musl' in text and '/usr/lib/x86_64-linux-musl' in text,'musl specs shape changed')
        relocated=relocate_musl_specs(root,work/'relocated-musl-gcc.specs')
        report['musl_specs']={'original_sha256':sha(specs),'relocated_sha256':sha(relocated)}
        call(common+['-DEXPECT_MUSL=1','-B'+str(root/'usr/lib/x86_64-linux-musl')+'/','-static','-no-pie','-specs='+str(relocated),src,'-o',muslout],'musl-compile')
        report['artifacts']=[]
        for name,artifact,dynamic in [('gnu',gnuout,True),('musl',muslout,False)]:
            h=call([readelf,'-hW',artifact],name+'-header');ph=call([readelf,'-lW',artifact],name+'-program');dyn=call([readelf,'-dW',artifact],name+'-dynamic')
            require('ELF64' in h and 'Advanced Micro Devices X86-64' in h,'wrong actual ELF machine')
            require(('INTERP' in ph)==dynamic and ('(NEEDED)' in dyn)==dynamic,'wrong actual dynamic/static linkage')
            report['artifacts'].append({'path':str(artifact),'sha256':sha(artifact),'bytes':artifact.stat().st_size,'dynamic':dynamic})
        report['emulation']={'performed':False,'reason':'no explicit qemu-x86_64 supplied'}
        if a.qemu_x86_64:
            require(a.qemu_x86_64.is_absolute() and a.qemu_x86_64.is_file() and os.access(a.qemu_x86_64,os.X_OK),'explicit emulator unavailable')
            for name,artifact in [('gnu',gnuout),('musl',muslout)]:
                result=call([a.qemu_x86_64,'-L',gnu,artifact],name+'-emulate')
                require(result=='MORUNA_CROSS_SMOKE_42\n','actual emulation output differs')
            report['emulation']={'performed':True,'executable':str(a.qemu_x86_64),'sha256':sha(a.qemu_x86_64)}
        elif a.require_emulation:raise RuntimeError('required explicit x86_64 emulator missing')
        report['passed']=True
    except BaseException as e:
        report['error']=str(e);raise
    finally:
        (work/'summary.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({'summary':str(work/'summary.json'),'passed':report['passed']}))
if __name__=='__main__':main()
