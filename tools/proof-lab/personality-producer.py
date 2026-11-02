#!/usr/bin/env python3
"""One Linux-only artifact producer, invoked solely by the existing owned runner."""
import argparse, importlib.util, json, os, pathlib, shlex, shutil, signal, subprocess, sys, time
P=pathlib.Path
spec=importlib.util.spec_from_file_location('verified_cross', '/tmp/cross-tools-linux-smoke.py')
cross=importlib.util.module_from_spec(spec);spec.loader.exec_module(cross)

CXX_WRAPPER = r"""#!/usr/bin/env python3
import hashlib,json,os,pathlib,subprocess,sys
config=__CONFIG__
argv=sys.argv[1:]
code=subprocess.call(config['compiler']+argv)
if code:sys.exit(code)
if '-c' not in argv:sys.exit(0)
if '-o' not in argv or argv.count('-o')!=1:raise RuntimeError('C++ compilation lacks one explicit output')
output=pathlib.Path(argv[argv.index('-o')+1])
with output.open('rb') as file:header=file.read(20)
if header[:6]!=b'\x7fELF\x02\x01' or header[16:20]!=b'\x01\x00\x3e\x00':raise RuntimeError('C++ output is not x86_64 ET_REL')
if output.is_symlink() or not output.is_file() or output.stat().st_size>16*1024**2:raise RuntimeError('C++ diagnostic object bound/type differs')
# Preserve actual ET_REL bytes BEFORE any undefined-symbol refusal. This is
# diagnostic custody only, never another produced runnable ELF/program.
data=output.read_bytes()
diagnostic=pathlib.Path(config['out'])/('cxx-object-'+str(os.getpid())+'.o')
fd=os.open(diagnostic,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'wb') as file:file.write(data);file.flush();os.fsync(file.fileno())
result=subprocess.run([config['nm'],'--undefined-only','--format=posix',str(output)],capture_output=True,text=True,check=True)
if len(result.stdout)>65536:raise RuntimeError('C++ undefined symbol bound exceeded')
symbols=[line.split()[0] for line in result.stdout.splitlines() if line.strip()]
unknown=sorted(set(symbols)-set(config['allowed']))
record={'object':str(output),'object_sha256':hashlib.sha256(data).hexdigest(),'diagnostic_path':diagnostic.name,'object_bytes':len(data),'elf':False,'artifact_type':'ET_REL diagnostic','undefined_symbols':symbols,'unknown_symbols':unknown,'nm_stdout':result.stdout,'compiler_sha256':config['compiler_sha256'],'inspected':True,'accepted':not unknown}
out=pathlib.Path(config['out'])/('cxx-inspect-'+str(os.getpid())+'.json')
with out.open('x') as file:json.dump(record,file,indent=2);file.write('\n')
if unknown:raise RuntimeError('C++ runtime/unexpected undefined symbols require actual musl runtime: '+repr(unknown))
"""

AUTHORED_CLEAN = r"""
import json,pathlib,subprocess
base=['rustup','run','1.98.1','cargo']
source=pathlib.Path('/proof/source')
stamp=pathlib.Path('/proof/target/.moruna-authored-clean-__CENSUS__.json')
expected={'inputs_census':'__CENSUS__','transition_sha256':'__TRANSITION__','target':'x86_64-unknown-linux-musl','policy':'host+explicit-target authored clean'}
if stamp.exists():
    if json.loads(stamp.read_text())!=expected:raise RuntimeError('Authored clean stamp differs')
    print('MORUNA_AUTHORED_CLEAN_RECEIPT_VERIFIED','__CENSUS__')
    raise SystemExit(0)
metadata=json.loads(subprocess.check_output(base+['metadata','--locked','--offline','--format-version','1'],text=True))
packages=[]
for package in metadata['packages']:
    if package['source'] is None:
        if not pathlib.Path(package['manifest_path']).is_relative_to(source):raise RuntimeError('Authored source escaped proof input')
        packages.append(package['name'])
if not packages:raise RuntimeError('No authored packages to invalidate')
if any(package['source'] is not None and package['name'] in packages for package in metadata['packages']):raise RuntimeError('Authored/external package name collision')
selection=[arg for package in packages for arg in ['--package',package]]
clean=base+['clean','--locked','--offline','--manifest-path','Cargo.toml']
subprocess.run(clean+selection,check=True)
subprocess.run(clean+['--target','x86_64-unknown-linux-musl']+selection,check=True)
temporary=stamp.with_suffix('.tmp');temporary.write_text(json.dumps(expected)+'\n');temporary.replace(stamp)
print('MORUNA_AUTHORED_ARTIFACTS_INVALIDATED',len(packages))
"""

def main():
    p=argparse.ArgumentParser();p.add_argument('--seconds',type=int,required=True);p.add_argument('--token',required=True)
    args=p.parse_args();cross.require(sys.platform=='linux' and 1<=args.seconds<=1050,'owned Linux bounded producer only')
    cross.require(len(args.token)==32 and all(c in '0123456789abcdef' for c in args.token),'invalid owner nonce')
    work=P('/proof/personality-work');work.mkdir(mode=0o700)
    out=P('/proof/target/personality-artifacts')/args.token;out.mkdir(parents=True,mode=0o700)
    deadline=time.monotonic()+args.seconds
    report={'version':1,'passed':False,'scope':'actual x86_64 Linux C/musl Rust artifacts only; NOT execution or kernel boot', 'commands':[], 'artifacts':[], 'rustup_metadata_modified':False}
    def checkpoint():
        temp=out/'progress.tmp';temp.write_text(json.dumps(report,indent=2)+'\n');temp.replace(out/'progress.json')
    def keep(source,name,elf=True,dynamic=False):
        cross.require(source.is_file() and not source.is_symlink() and source.stat().st_size<=128*1024**2,'artifact bound/type differs')
        dest=out/name;temp=out/(name+'.tmp');shutil.copyfile(source,temp);temp.chmod(0o700 if elf else 0o600)
        with temp.open('rb') as file:os.fsync(file.fileno())
        temp.replace(dest)
        report['artifacts'].append({'name':name,'bytes':dest.stat().st_size,'sha256':cross.sha(dest),'elf':elf,'elf_verified':False if elf else None,'dynamic':dynamic});checkpoint()
    env=dict(os.environ);env.update(CARGO_HOME='/proof/cargo',CARGO_TARGET_DIR='/proof/target',CARGO_NET_OFFLINE='true',CARGO_BUILD_JOBS='1')
    env.pop('CARGO_ENCODED_RUSTFLAGS',None);env['RUSTFLAGS']='--cfg polyval_force_soft'
    def call(argv,name,environment=None):
        cross.require(time.monotonic()<deadline,'producer deadline expired')
        log=out/(name+'.log');child=None
        try:
            with log.open('xb') as output:
                child=subprocess.Popen([str(x) for x in argv],cwd='/proof/source',env=environment or env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
                while child.poll() is None:
                    cross.require(time.monotonic()<deadline,'producer deadline expired')
                    cross.require(log.stat().st_size<=16*1024**2,'producer log bound exceeded');time.sleep(.1)
            cross.require(child.returncode==0,'artifact command failed: '+name)
        finally:
            if child is not None and child.poll() is None:
                os.killpg(child.pid,signal.SIGTERM)
                try:child.wait(timeout=3)
                except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=3)
            report['commands'].append({'name':name,'argv':[str(x) for x in argv],'exit':None if child is None else child.returncode,'log':log.name,'sha256':cross.sha(log) if log.exists() else None});checkpoint()
        return log.read_text()
    try:
        bundle=json.loads(P('/inputs/cross/bundle.json').read_text());rust=json.loads(P('/tmp/rust-bundle.json').read_text())
        cross.require(bundle['archive_sha256']=='f24b0cfa8edae9aa75f312595088fd44183a2417afed75c9bc8ac3051098f94f' and cross.sha(P('/inputs/cross/cross-tools.tar.gz'))==bundle['archive_sha256'],'cross archive differs')
        tree=cross.manifest(P('/inputs/cross/manifest.json'),bundle['manifest_sha256'])
        tools=work/'tools';cross.extract(P('/inputs/cross/cross-tools.tar.gz'),tree,tools,bundle)
        rust_tree=cross.manifest(P('/tmp/rust-manifest.json'),rust['manifest_sha256'])
        report['rust_libraries']=cross.verify_rust(P('/inputs/musl'),rust_tree,rust)
        common=cross.compiler_arguments(tools);gnu=tools/'usr/x86_64-linux-gnu';libgcc=tools/'usr/lib/gcc-cross/x86_64-linux-gnu/12'
        relocated=cross.relocate_musl_specs(tools,work/'musl.specs')
        env['LD_LIBRARY_PATH']=str(tools/'usr/lib/aarch64-linux-gnu')+':'+env.get('LD_LIBRARY_PATH','')
        # Exactly the GNU/musl paths of the existing smoke helper/build entry.
        gnu_args=common+['-B'+str(gnu/'lib')+'/','-nostdinc','-isystem',libgcc/'include','-isystem',gnu/'include','-L'+str(gnu/'lib')]
        musl_args=common+['-B'+str(tools/'usr/lib/x86_64-linux-musl')+'/','-static','-no-pie','-specs='+str(relocated)]
        readelf=tools/'usr/bin/x86_64-linux-gnu-readelf'
        def elf(source,name,dynamic):
            keep(source,name,True,dynamic)
            h=call([readelf,'-hW',source],name+'-header');ph=call([readelf,'-lW',source],name+'-program');dy=call([readelf,'-dW',source],name+'-dynamic')
            cross.require('ELF64' in h and 'Advanced Micro Devices X86-64' in h,'wrong actual ELF target')
            cross.require(('INTERP' in ph)==dynamic and ('(NEEDED)' in dy)==dynamic,'wrong actual static/dynamic linkage')
            report['artifacts'][-1]['elf_verified']=True;checkpoint()
        reuse=json.loads(P('/tmp/personality-reuse.json').read_text()) if P('/tmp/personality-reuse.json').exists() else None
        if reuse:
            prior=P(reuse['guest_directory']);report['reused_from']=reuse
            for item in reuse['artifacts']:
                source=prior/item['name']
                cross.require(not source.is_symlink() and source.is_file() and source.stat().st_size==item['bytes'] and cross.sha(source)==item['sha256'],'prior actual artifact changed')
                keep(source,item['name'],True,item['dynamic']);report['artifacts'][-1]['elf_verified']=True;report['artifacts'][-1]['reused']=True;checkpoint()
        for name in (() if reuse else ('hello','threads','refuse','isolate','bench','granted','hog','tap','badrecord')):
            output=work/name;call(musl_args+['-Wall','-Werror','-s','-o',output,P('/proof/source/kernel/linux/programs')/(name+'.c')],name+'-compile');elf(output,name,False)
        if not reuse:
            output=work/'dynamic';call(gnu_args+['-Wall','-Werror','-s','-o',output,'kernel/linux/programs/dynamic.c'],'dynamic-compile');elf(output,'dynamic',True)
        sysroot=work/'rust-sysroot';(sysroot/'lib/rustlib').mkdir(parents=True);(sysroot/'lib/rustlib/x86_64-unknown-linux-musl').symlink_to('/inputs/musl')
        # Real private libraries, no rustup components metadata or compiler replacement.
        wrapper=work/'gnu-linker';wrapper.write_text('#!/bin/sh\nexec '+shlex.join([str(x) for x in common])+' "$@"\n');wrapper.chmod(0o700)
        cc=work/'musl-cc';cc.write_text('#!/bin/sh\nexec '+shlex.join([str(x) for x in musl_args])+' "$@"\n');cc.chmod(0o700)
        env.update(CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=str(wrapper),CC_x86_64_unknown_linux_musl=str(cc),AR_x86_64_unknown_linux_musl=str(tools/'usr/bin/x86_64-linux-gnu-ar'))
        env['RUSTFLAGS']='--cfg polyval_force_soft --sysroot='+str(sysroot)+' -C link-self-contained=yes -C target-feature=+crt-static -C strip=symbols'
        version=call(['rustup','run','1.98.1','rustc','--version'],'rust-version')
        cross.require(version.strip().startswith('rustc 1.98.1 (48a229cea'),'actual compiler does not match official musl libraries')
        base=['rustup','run','1.98.1','cargo','build','--locked','--offline','--release','--target','x86_64-unknown-linux-musl','--config','profile.release.debug=0','--config','profile.release.incremental=false','-j1']

        # Genuine Clang C++ frontend and target C headers from the verified bundles.
        clang=P('/opt/moruna-proof-tools/usr/bin/clang++-14')
        resource=P('/opt/moruna-proof-tools/usr/lib/llvm-14/lib/clang/14.0.6')
        cross.require(clang.is_file() and (resource/'include/stddef.h').is_file(),'explicit verified Clang14/resource input missing')
        target_args=['--target=x86_64-unknown-linux-musl','--sysroot='+str(tools),'-nostdinc','-nostdinc++','-isystem',str(tools/'usr/include/x86_64-linux-musl'),'-isystem',str(resource/'include')]
        allowed=['__assert_fail','__errno_location','malloc','calloc','realloc','free','memcpy','memmove','memset','memcmp','__tls_get_addr','_GLOBAL_OFFSET_TABLE_','__stack_chk_fail']
        # Clang may lower memcmp equality to the C libc bcmp symbol. Admit it
        # only after inspecting the ACTUAL verified target libc definition.
        libc=P('/inputs/musl/lib/self-contained/libc.a')
        definitions=call([tools/'usr/bin/x86_64-linux-gnu-nm','-g','--defined-only','--format=posix',libc],'musl-libc-defined')
        bcmp=[line for line in definitions.splitlines() if len(line.split())>=2 and line.split()[0]=='bcmp' and line.split()[1]=='T']
        cross.require(len(bcmp)==1,'actual verified musl libc must define exactly one global bcmp function')
        header=tools/'usr/include/x86_64-linux-musl/strings.h'
        cross.require('int bcmp (const void *, const void *, size_t);' in header.read_text(),'verified musl bcmp declaration missing')
        report['musl_bcmp']={'libc_sha256':cross.sha(libc),'header_sha256':cross.sha(header),'actual_defined_symbol':bcmp[0],'defined_symbols_log':'musl-libc-defined.log'}
        allowed.append('bcmp');checkpoint()
        config={'compiler':[str(clang),*target_args],'compiler_sha256':cross.sha(clang),'nm':str(tools/'usr/bin/x86_64-linux-gnu-nm'),'allowed':allowed,'out':str(out)}
        cxx=work/'clang-cxx';cxx.write_text(CXX_WRAPPER.replace('__CONFIG__',repr(config)));cxx.chmod(0o700)
        source=P('/proof/source/vendor/custom-labels-0.4.6/src/customlabels.cpp')
        report['cxx_preflight']={'compiler_sha256':config['compiler_sha256'],'sources':{p.name:cross.sha(p) for p in [source,source.with_name('customlabels.h'),source.with_name('util.h')]},'target_args':target_args,'stdlib_omitted':False};checkpoint()
        obj=work/'customlabels-preflight.o'
        call([cxx,'-std=c++17','-O3','-ffunction-sections','-fdata-sections','-fPIC','-m64','-Wall','-Wextra','-c',source,'-o',obj],'customlabels-preflight')
        # This is ET_REL diagnostic custody, NEVER another runnable ELF/program.
        keep(obj,'customlabels-preflight.o',False)
        report['cxx_preflight']['stdlib_omitted']=True;checkpoint()
        env.update(CXX_x86_64_unknown_linux_musl=str(cxx),CXXSTDLIB_x86_64_unknown_linux_musl='',CLANG_PATH=str(clang))
        env['CXX_x86_64-unknown-linux-musl']=str(cxx)
        env['CXXSTDLIB_x86_64-unknown-linux-musl']=''
        # cc1.4.7 explicitly interprets empty target CXXSTDLIB as no link library.
        # Every real Cargo C++ compiled object is still checked by the same wrapper.
        bindgen=shlex.join([arg for arg in target_args if arg!='-nostdinc++'])
        env['BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_musl']=bindgen
        env['BINDGEN_EXTRA_CLANG_ARGS_x86_64-unknown-linux-musl']=bindgen
        full_clean=json.loads(P('/tmp/personality-full-clean.json').read_text()) if P('/tmp/personality-full-clean.json').exists() else None
        if (reuse and reuse.get('transition')) or full_clean:
            cross.require(not(reuse and full_clean),'full rebuild cannot retain prior artifacts')
            transition=reuse['transition'] if reuse else full_clean
            clean=AUTHORED_CLEAN.replace('__CENSUS__',transition['new_inputs_census_sha256']).replace('__TRANSITION__',transition['sha256'])
            clean_env=dict(env);clean_env['RUSTFLAGS']='--cfg polyval_force_soft'
            call([sys.executable,'-c',clean],'authored-clean',clean_env)
            report['authored_clean']={'inputs_census_sha256':transition['new_inputs_census_sha256'],'transition_sha256':transition['sha256'],'host_and_target':'x86_64-unknown-linux-musl','external_dependencies_retained':True};checkpoint()
        if P('/tmp/layers-tests.json').exists():
            import tarfile
            layers=json.loads(P('/tmp/layers-tests.json').read_text());archive=P('/inputs/layers-kernel')/layers['archive']
            cross.require(cross.sha(archive)==layers['sha256'],'focused test vendor archive changed')
            vendor=work/'layers-vendor';vendor.mkdir(mode=0o700)
            with tarfile.open(archive) as tar:
                for entry in tar:
                    path=P(entry.name)
                    if not path.parts or path.parts[0]!='vendor':continue
                    cross.require(entry.isfile() and not path.is_absolute() and '..' not in path.parts,'invalid vendor archive member')
                    dest=vendor/path;dest.parent.mkdir(parents=True,exist_ok=True)
                    with tar.extractfile(entry) as source,dest.open('xb') as target:shutil.copyfileobj(source,target)
            testenv=dict(env);testenv['CARGO_TARGET_DIR']=str(work/'layers-target');testenv['RUSTFLAGS']='--cfg polyval_force_soft'
            # This ARM host model uses the exact shipping client helper; no native feature.
            for key in list(testenv):
                if key.startswith(('CXX','CC_','BINDGEN_EXTRA_CLANG_ARGS')):testenv.pop(key)
            call(['/usr/bin/timeout','90','rustup','run','1.98.1','cargo','test','--locked','--offline','--manifest-path','kernel/Cargo.toml','--target','aarch64-unknown-linux-gnu','--config','source.vendored-sources.directory='+json.dumps(str(vendor/'vendor')),'--config','profile.dev.debug=0','--config','profile.dev.incremental=false','--config','profile.test.debug=0','--config','profile.test.incremental=false','-p','linux-protocol','--test','layers','-j1'],'layers-client-tests',testenv)
            report['layers_client_tests']={'passed':True,'target':'aarch64-unknown-linux-gnu','guest_local_target':testenv['CARGO_TARGET_DIR'],'vendor_archive_sha256':layers['sha256']};checkpoint()
        if P('/tmp/layers-tests.json').exists():
            accountenv=dict(env);accountenv['CARGO_TARGET_DIR']=str(work/'account-tests-target');accountenv['RUSTFLAGS']='--cfg polyval_force_soft'
            for key in list(accountenv):
                if key.startswith(('CXX','CC_','BINDGEN_EXTRA_CLANG_ARGS')):accountenv.pop(key)
            testbase=['/usr/bin/timeout','180','rustup','run','1.98.1','cargo','test','--locked','--offline','--target','aarch64-unknown-linux-gnu','--config','profile.dev.debug=0','--config','profile.dev.incremental=false','--config','profile.test.debug=0','--config','profile.test.incremental=false','-j1']
            call(testbase+['-p','moruna-kernel','--test','ct_native_account'],'account-sampler-tests',accountenv)
            call(testbase+['-p','moruna-trace','--test','tr_t3_pure_report'],'account-report-tests',accountenv)
            call(testbase+['-p','moruna-controller','--lib','admission::tests'],'controller-admission-tests',accountenv)
            report['account_tests']={'passed':True,'target':'aarch64-unknown-linux-gnu','guest_local_target':accountenv['CARGO_TARGET_DIR'],'tests':['ct_native_account','tr_t3_pure_report','moruna-controller::admission::tests']};checkpoint()
        for package,name in ([('moruna-runtime','moruna-engine')] if reuse else [('moruna-image','arrow_echo'),('moruna-runtime','moruna-engine')]):
            call(base+['-p',package,'--bin',name],name+'-build',env);elf(P('/proof/target/x86_64-unknown-linux-musl/release')/name,name,False)
        for name in ('transform.py','arrowlite.py'):keep(P('/proof/source/kernel/linux/programs/kd2')/name,name,False)
        keep(P('/proof/source/kernel/linux/programs/greeting.txt'),'greeting.txt',False)
        keep(relocated,'relocated-musl.specs',False)
        report['passed']=True
    except BaseException as error:
        report['error']=type(error).__name__+': '+str(error);raise
    finally:checkpoint()
if __name__=='__main__':main()
