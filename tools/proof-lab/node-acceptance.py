#!/usr/bin/env python3
"""Private offline adapter for existing node acceptance commands; no compilation.

Producer records retain their own source identities. The input index only selects
actual successful retained records; it is not a substitute producer manifest.
"""
import argparse,datetime,hashlib,io,json,os,re,shutil,signal,stat,sys,tarfile,time
from pathlib import Path
from attachments import private_directory,flags,validate_inspect
from run import ROOT,Run,LABEL,require,sha
ROLES=('native','personality','host','image')
HOST={'moruna-vhost-vsock','kd3-peer','node-ci-assets','kd3-native-proof','moruna-storage-provision','moruna-layer','plane-fixture','kd2-fixture'}
IMAGE={'moruna-image','moruna-node-image'}
SCRIPT_NAMES={'kernel/node/tools/proof.py','kernel/node/tools/io-proof.py','kernel/node/tools/configure.py','kernel/node/standalone.image','kernel/tools/oci-manifest.py','kernel/tools/net-proof.py','kernel/tools/check-node-fixtures.py','kernel/tools/node-fixtures.sh','kernel/tools/build.sh'}

def private_file(raw):
    p=Path(raw).absolute();require(p.is_relative_to(ROOT),'file outside owned proof root');private_directory(p.parent);z=p.lstat()
    require(stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1,'file not owned regular single-link input')
    return p

def atomic_json(p,v):
    tmp=p.with_name(p.name+'.new');require(not tmp.exists(),'pending journal update requires review')
    with tmp.open('x') as f:json.dump(v,f,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
    os.replace(tmp,p)
    fd=os.open(p.parent,os.O_RDONLY)
    try:os.fsync(fd)
    finally:os.close(fd)

def file_census(root,limit=8*1024**3,allow_sockets=False):
    private_directory(root);rows=[];total=0
    for base,dirs,files in os.walk(root,followlinks=False):
        for n in dirs:require(not (Path(base)/n).is_symlink(),'output directory symlink')
        for n in files:
            p=Path(base)/n;z=p.lstat()
            if allow_sockets and stat.S_ISSOCK(z.st_mode):
                require(z.st_uid==os.getuid(),'foreign residual socket');rows.append({'path':p.relative_to(root).as_posix(),'type':'socket','device':z.st_dev,'inode':z.st_ino,'mode':stat.S_IMODE(z.st_mode)});continue
            require(stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1,'output must be owned regular file')
            total+=z.st_size;require(total<=limit and len(rows)<100000,'retained output bound')
            rows.append({'path':p.relative_to(root).as_posix(),'bytes':z.st_size,'mode':stat.S_IMODE(z.st_mode),'sha256':sha(p)})
    return sorted(rows,key=lambda e:e['path'])

def validate_index(raw):
    index=private_file(raw);v=json.loads(index.read_text());require(v.get('version')==1,'input index version differs')
    require(set(v['producers'])==set(ROLES),'all four real producers required')
    fingerprints=[];mounts=[];producers={};selected={}
    def fingerprint(p):
        p=private_file(p);z=p.stat();e={'path':str(p),'bytes':z.st_size,'sha256':sha(p),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)};fingerprints.append(e);return p
    fingerprint(index)
    for role in ROLES:
        ref=v['producers'][role];sp=fingerprint(ref['summary']);mp=fingerprint(ref['manifest']);root=private_directory(Path(ref['root']));summary=json.loads(sp.read_text());manifest=json.loads(mp.read_text())
        require(summary.get('cleanup') is True and summary.get('stage_passed') is True and summary.get('guest_exit')==0,'producer lacks actual pass/cleanup: '+role)
        if role=='native':
            require(summary.get('phase')=='link' and isinstance(manifest,list),'native actual link record required')
            entries=manifest;require(len(entries)==52,'actual native52 census required')
        elif role=='personality':
            require(summary.get('phase')=='personality-artifacts' and manifest.get('passed') is True,'actual personality producer required');entries=manifest['artifacts']
        else:
            require(summary.get('phase')==('boot-host-tools' if role=='host' else 'image-tools'),'wrong actual helper producer phase');entries=manifest['files']
            require({e['name'] for e in entries}==(HOST if role=='host' else IMAGE),'actual helper set incomplete')
        rows=[];names=set()
        for e in entries:
            name=e['name'];require(name not in names and '/' not in name and name not in ('.','..'),'duplicate/unsafe producer name');names.add(name)
            rel=(e['target']+'/'+name) if role=='native' else name
            p=fingerprint(root/rel);require(p.stat().st_size==e['bytes'] and sha(p)==e['sha256'],'actual shipped artifact changed: '+role+'/'+rel)
            rows.append({'name':name,'relative':rel,'bytes':e['bytes'],'sha256':e['sha256']})
        if role=='personality':require({'moruna-engine','arrow_echo','hello','threads','refuse','isolate','bench','granted','hog','tap','badrecord','dynamic','transform.py','arrowlite.py','greeting.txt'}<=names,'personality12 and actual source payload missing')
        producers[role]={'summary':summary,'summary_sha256':sha(sp),'manifest_sha256':sha(mp),'root':str(root),'summary_path':str(sp),'manifest_path':str(mp),'files':rows}
        selected[role]=rows;mounts.append({'path':str(root),'destination':'/inputs/'+role,'readonly':True})
    driver_override=None
    if 'driver_override' in v:
        ref=v['driver_override'];sp=fingerprint(ref['summary']);mp=fingerprint(ref['manifest']);root=private_directory(Path(ref['root']))
        summary=json.loads(sp.read_text());manifest=json.loads(mp.read_text())
        require(summary.get('phase')=='sdk-proof-build' and summary.get('cleanup') is True and summary.get('stage_passed') is True and summary.get('guest_exit')==0,'driver override requires actual passed SDK-only producer')
        require(sp.parent/root.name==root and mp==root/'boot-host-tools.json' and root.name=='witness-proof','driver producer output paths differ')
        require(manifest.get('profile')=='dev-debug0' and manifest.get('target')=='aarch64-unknown-linux-gnu' and manifest.get('toolchain')=='1.98.1' and manifest.get('jobs')==1 and len(manifest['files'])==1,'driver override compiler/profile/census differs')
        row=manifest['files'][0];require(row['name']=='kd3-native-proof','driver override must contain only driver')
        binary=fingerprint(root/row['name']);header=binary.read_bytes()[:20]
        require(binary.stat().st_mode&0o111 and 0<row['bytes']<=128*1024**2 and binary.stat().st_size==row['bytes'] and sha(binary)==row['sha256'] and header[:6]==b'\x7fELF\x02\x01' and header[18:20]==b'\xb7\x00','driver actual ARM64 ELF/hash differs')
        require(0<row['original_bytes']<=512*1024**2 and len(row['original_sha256'])==len(row['strip_tool_sha256'])==64 and row['strip_version'],'driver raw/strip custody missing')
        require(Path(producers['host']['summary_path'])==ROOT/'moruna-proof-501-512022cbe2e0/summary.json','driver override requires exact actual512 basehost producer')
        host_summary=producers['host']['summary'];stable_cache=('owner_nonce','device','inode','toolchain','target','profile','source_lock_sha256','destination','readonly')
        require(summary['pins']==host_summary['pins'] and summary['readonly_tools']==host_summary['readonly_tools'] and all(summary['cache'][k]==host_summary['cache'][k] for k in stable_cache),'driver compiler/tools/owned cache identity changed')
        provenance=fingerprint(ref['source_provenance']);new=json.loads(provenance.read_text());basep=fingerprint(ROOT/'combined-inputs-ls15b21h/provenance.json');base=json.loads(basep.read_text())
        require(producers['host']['summary']['inputs_census_sha256']==base['inputs_census_sha256'] and producers['host']['summary']['source_archive_sha256']==base['archive_sha256'],'512 basehost source binding differs')
        require(summary['source_archive_sha256']==new['archive_sha256'] and summary['inputs_census_sha256']==new['inputs_census_sha256'] and new['cargo_lock_sha256']==base['cargo_lock_sha256'] and new['portable_vendor_config_sha256']==base['portable_vendor_config_sha256'],'driver source/compiler binding differs')
        before_manifest=fingerprint(base['inputs_census_path']);after_manifest=fingerprint(new['inputs_census_path'])
        require(sha(before_manifest)==base['inputs_census_sha256'] and sha(after_manifest)==new['inputs_census_sha256'],'driver source census SHA differs')
        before={e['path']:e for e in json.loads(before_manifest.read_text())};after={e['path']:e for e in json.loads(after_manifest.read_text())}
        name='crates/moruna-runtime/examples/kd3-native-proof.rs'
        sink_tests=None
        if summary.get('nonce')=='298df6800d5f3629017eeb931a0367e3':
            require(set(before)==set(after) and {n for n in before if before[n]!=after[n]}=={name},'historical298 source changed beyond buffered example')
            require(before[name]['sha256']=='68b2fe0428663a1599f0d28d7820fdb62b4166c1e44c906ea74dc6414b7ce03a' and after[name]['sha256']=='a7884f43efc85692a6cf0a4cf1b2a4a2ac25cb60314ce04ffb5629d96bbea1ae' and row['sha256']=='f65f04a743f2bb67c25fc1fd73623960240288fce8b1d45c3b50698c26f18159','historical298 buffered source/ELF differs')
        else:
            expected_sink={'crates/moruna-runtime/examples/kd3-native-proof.rs': [{'bytes': 12860, 'mode': 420, 'path': 'crates/moruna-runtime/examples/kd3-native-proof.rs', 'sha256': '68b2fe0428663a1599f0d28d7820fdb62b4166c1e44c906ea74dc6414b7ce03a', 'type': 'file'}, {'path': 'crates/moruna-runtime/examples/kd3-native-proof.rs', 'type': 'file', 'mode': 420, 'bytes': 12988, 'sha256': 'a7884f43efc85692a6cf0a4cf1b2a4a2ac25cb60314ce04ffb5629d96bbea1ae'}], 'crates/moruna-runtime/examples/kd3/common.rs': [{'bytes': 12467, 'mode': 420, 'path': 'crates/moruna-runtime/examples/kd3/common.rs', 'sha256': 'be730393bee2bd40226a53fac088cdcdd59519a08cbe63771bcf13df03329c45', 'type': 'file'}, {'path': 'crates/moruna-runtime/examples/kd3/common.rs', 'type': 'file', 'mode': 420, 'bytes': 13023, 'sha256': '796d93b2817dce51572e149d709887ea523a671e8d7be3e0bf2fe8e1673b48a1'}], 'crates/moruna-runtime/examples/kd3/sink_ack.rs': [None, {'path': 'crates/moruna-runtime/examples/kd3/sink_ack.rs', 'type': 'file', 'mode': 420, 'bytes': 3664, 'sha256': '04bf0af929952cb88e8df595ce1e95bbe35dbfa3a5e4ef916ec584865cff9ba7'}], 'crates/moruna-event/tests/records.rs': [{'bytes': 24401, 'mode': 420, 'path': 'crates/moruna-event/tests/records.rs', 'sha256': '6b7a43b5fe085b27633c736ea8a0c9608fea399226e46ac34a5406da585d1247', 'type': 'file'}, {'path': 'crates/moruna-event/tests/records.rs', 'type': 'file', 'mode': 420, 'bytes': 31007, 'sha256': '3561a957134c3c39599b69788113589362b61d3ffb91b472ea48cfca528e3545'}]}
            common_name='crates/moruna-runtime/examples/kd3/common.rs'
            failurefix=after.get(common_name,{}).get('sha256')=='d4f5ac087c1a8517143629b06bd44b7f77f036f0f1d2a94c22927002ccb55720'
            if failurefix:
                expected_sink[common_name][1]={**expected_sink[common_name][1],'bytes':14103,'sha256':'d4f5ac087c1a8517143629b06bd44b7f77f036f0f1d2a94c22927002ccb55720'}
            require(not(set(before)-set(after)) and set(after)-set(before)=={'crates/moruna-runtime/examples/kd3/sink_ack.rs'},'sink criterion source additions/removals differ')
            require({n for n in before.keys()|after.keys() if before.get(n)!=after.get(n)}==set(expected_sink),'sink criterion changes source outside exact four paths fromls15')
            for n,(old_record,new_record) in expected_sink.items():require(before.get(n)==old_record and after.get(n)==new_record,'sink criterion full source record differs')
            if failurefix:
                anchor_summary=fingerprint(ROOT/'moruna-proof-501-86ac3876204d/summary.json');anchor=json.loads(anchor_summary.read_text())
                anchor_provenance=fingerprint(ROOT/'combined-inputs-wdupjht5/provenance.json');anchor_source=json.loads(anchor_provenance.read_text());anchor_manifest=fingerprint(anchor_source['inputs_census_path'])
                require(anchor.get('cleanup') is True and anchor.get('stage_passed') is True and anchor.get('guest_exit')==0 and anchor.get('phase')=='sdk-proof-build' and anchor['inputs_census_sha256']==anchor_source['inputs_census_sha256'] and anchor['source_archive_sha256']==anchor_source['archive_sha256'] and sha(anchor_manifest)==anchor_source['inputs_census_sha256'],'actual passed86ac source anchor differs')
                prior_records={e['path']:e for e in json.loads(anchor_manifest.read_text())}
                require(set(prior_records)==set(after) and {n for n in prior_records if prior_records[n]!=after[n]}=={common_name},'failure diagnostic fix must be exactONEcommon delta from actualpassed86ac')
                require(prior_records[common_name]=={**expected_sink[common_name][1],'bytes':13023,'sha256':'796d93b2817dce51572e149d709887ea523a671e8d7be3e0bf2fe8e1673b48a1'},'failure diagnostic fix old common record differs')
            else:
                anchor_summary=fingerprint(ROOT/'moruna-proof-501-298df6800d5f/summary.json');anchor=json.loads(anchor_summary.read_text())
                anchor_provenance=fingerprint(ROOT/'combined-inputs-_chzmbrc/provenance.json');anchor_source=json.loads(anchor_provenance.read_text());anchor_manifest=fingerprint(anchor_source['inputs_census_path'])
                require(anchor.get('cleanup') is True and anchor.get('stage_passed') is True and anchor.get('guest_exit')==0 and anchor.get('phase')=='sdk-proof-build' and anchor['inputs_census_sha256']==anchor_source['inputs_census_sha256']=='89a8c1de77321841f25b24397ffd99693e50c9b692a5cab47dbdf911061109c8' and anchor['source_archive_sha256']==anchor_source['archive_sha256']=='c253a647e0e54fb654ca2b6ed1164ab6eb13bc2af26927b1e8efd1c51f2a1def' and sha(anchor_manifest)==anchor_source['inputs_census_sha256'],'actual passed298 source anchor differs')
                prior_records={e['path']:e for e in json.loads(anchor_manifest.read_text())}
                expected_changed=set(expected_sink)-{name}
                require(not(set(prior_records)-set(after)) and set(after)-set(prior_records)=={'crates/moruna-runtime/examples/kd3/sink_ack.rs'} and {n for n in prior_records.keys()|after.keys() if prior_records.get(n)!=after.get(n)}==expected_changed,'sink criterion must be exact3delta fromactual298 source')
            tests=manifest.get('focused_tests');require(tests is not None and tests.get('command')==['cargo','test','-p','moruna-event','--test','records','kd3_sink_ack_tests'] and (tests.get('passed'),tests.get('failed'),tests.get('ignored'),tests.get('exit'))==(6,0,0,0) and tests.get('log')=='sink-ack-tests.log','actual six sink criterion tests required')
            log=fingerprint(root/tests['log']);require(log.stat().st_size<=8*1024**2 and sha(log)==tests['sha256'],'actual sink test log SHA differs')
            counts=re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log.read_text(),re.MULTILINE);require(counts==[('6','0','0')],'sink tests must allpass, zeroignored')
            sink_tests={**tests,'log_path':str(log),'anchor_summary':str(anchor_summary),'anchor_summary_sha256':sha(anchor_summary),'retained_peer':'actual512 old template emitter only; transport/reducer not used bySDK branch'}
        old=next(e for e in selected['host'] if e['name']=='kd3-native-proof');replacement={**old,'bytes':row['bytes'],'sha256':row['sha256']}
        selected['host']=[replacement if e['name']=='kd3-native-proof' else e for e in selected['host']]
        mounts.append({'path':str(root),'destination':'/inputs/driver-override','readonly':True})
        driver_override={'sink_ack_tests':sink_tests,'source':str(binary),'source_mode':stat.S_IMODE(binary.stat().st_mode),'bytes':row['bytes'],'sha256':row['sha256'],'old_sha256':old['sha256'],'old_bytes':old['bytes'],'summary':str(sp),'summary_sha256':sha(sp),'manifest':str(mp),'manifest_sha256':sha(mp),'source_provenance':str(provenance),'source_provenance_sha256':sha(provenance),'raw_artifact':row,'base_host_summary_sha256':producers['host']['summary_sha256']}
    o=v['python_oci'];bp=fingerprint(o['bundle']);bundle=json.loads(bp.read_text());root=private_directory(Path(bundle['root']));mp=fingerprint(bundle['manifest']);require(sha(mp)==bundle['manifest_sha256'] and bundle['platform']=='linux/amd64','actual Python OCI bundle differs')
    z=root.stat();require((z.st_dev,z.st_ino,z.st_uid)==(bundle['device'],bundle['inode'],bundle['uid']),'OCI root identity differs')
    expected=json.loads(mp.read_text());actual=file_census(root,512*1024**2)
    require(len(expected)==bundle['files'] and sum(e['bytes'] for e in expected)==bundle['bytes'],'OCI totals differ')
    require({e['path']:(e['bytes'],e['sha256']) for e in actual}=={e['path']:(e['bytes'],e['sha256']) for e in expected},'OCI bytes/census differ')
    for e in actual:fingerprint(root/e['path'])
    mounts.append({'path':str(root),'destination':'/inputs/python-oci','readonly':True})
    scripts={};script_sources=[]
    for entry in v['scripts']:
        n=entry['relative'];require(n in SCRIPT_NAMES and n not in scripts,'unexpected/duplicate script')
        p=fingerprint(entry['path']);m=fingerprint(entry['source_manifest']);rows=json.loads(m.read_text());row=next((e for e in rows if e['path']==n),None)
        require(row is not None and row['type']=='file' and row['bytes']==p.stat().st_size and row['sha256']==sha(p),'script does not match its own captured source census')
        scripts[n]=p;script_sources.append({'relative':n,'sha256':sha(p),'source_manifest':str(m),'source_manifest_sha256':sha(m)})
    require(set(scripts)==SCRIPT_NAMES,'exact existing command scripts required')
    tls=fingerprint(v['tls_roots']['path']);require(sha(tls)==v['tls_roots']['sha256'],'TLS roots differ')
    tls_provenance=fingerprint(v['tls_roots']['provenance']) if 'provenance' in v['tls_roots'] else None
    require(v['qemu']['path'].startswith('/') and len(v['qemu']['sha256'])==64,'explicit actual guest QEMU path/hash required')
    directory_fingerprints=[];link_fingerprints=[];qemu_bundle=None
    if 'qemu_bundle' in v:
        q=v['qemu_bundle'];root=private_directory(Path(q['root']));mp=fingerprint(q['manifest']);sp=fingerprint(q['summary'])
        require(sha(mp)==q['manifest_sha256'],'private QEMU manifest changed')
        summary=json.loads(sp.read_text());require(summary.get('mode')=='qemu-vsock-build' and summary.get('passed') is True and summary.get('cleanup') is True and summary.get('guest_exit')==0,'private QEMU lacks actual successful build/device verification/cleanup')
        z=root.stat();require(Path(summary['output'])==root and tuple(summary['output_identity'])==(z.st_dev,z.st_ino),'private QEMU producer root identity differs')
        record=json.loads(mp.read_text());require(mp==root/'.proof-output.json' and record.get('version')==1 and record.get('target')=='x86_64-softmmu' and record.get('host')=='aarch64-linux' and record.get('prefix')=='/proof/qemu-common/install' and record.get('source_sha256')=='a3717477d8e2c84d630bfffbc20f6cd3293eb45aa1e6dac6d0cc27689991c9e1','private QEMU actual manifest contract differs')
        require(summary.get('manifest_sha256')==sha(mp) and summary.get('artifacts')==record,'private QEMU independent postcleanup manifest differs')
        rows=record['files'];require(isinstance(rows,list) and len(rows)<=50000,'private QEMU manifest bound')
        expected={}
        for e in rows:
            n=e['path'];parts=Path(n).parts
            require(n and not Path(n).is_absolute() and '..' not in parts and str(Path(n))==n and n not in expected,'unsafe/duplicate QEMU member')
            require(e['type'] in ('file','symlink','directory'),'unsupported private QEMU member')
            expected[n]=e
        actual=set();total=0
        for base,dirs,files in os.walk(root,followlinks=False):
            for name in dirs+files:
                path=Path(base)/name;rel=path.relative_to(root).as_posix();z=path.lstat();e=expected.get(rel)
                if path==mp:continue # Sole producer's self-excluded manifest is independently fingerprinted.
                require(z.st_uid==os.getuid(),'private QEMU member owner differs')
                if stat.S_ISDIR(z.st_mode):
                    if e is not None:require(e['type']=='directory','private QEMU directory differs');actual.add(rel)
                    continue
                require(e is not None,'extra private QEMU input');actual.add(rel)
                if stat.S_ISLNK(z.st_mode):
                    require(e['type']=='symlink' and os.readlink(path)==e['target'] and path.resolve(strict=True).is_relative_to(root),'private QEMU link differs/escapes')
                    link_fingerprints.append({'path':str(path),'device':z.st_dev,'inode':z.st_ino,'uid':z.st_uid,'target':e['target']})
                else:
                    require(e['type']=='file' and stat.S_ISREG(z.st_mode) and z.st_nlink==1 and z.st_size==e['bytes'] and sha(path)==e['sha256'],'private QEMU bytes differ')
                    if 'mode' in e:require(stat.S_IMODE(z.st_mode)==e['mode'],'private QEMU mode differs')
                    total+=z.st_size;require(total<=1024**3,'private QEMU byte bound');fingerprint(path)
        require(actual==set(expected) and total==record['bytes'],'private QEMU file census/bytes differ')
        require((root/'version.txt').read_text().startswith('QEMU emulator version 10.2.1'),'private QEMU is not the same reviewed release')
        z=root.stat();directory_fingerprints.append({'path':str(root),'device':z.st_dev,'inode':z.st_ino,'uid':z.st_uid})
        executable=root/'bin/qemu-system-x86_64'
        require(v['qemu']['path']=='/proof/qemu-common/install/bin/qemu-system-x86_64' and not executable.is_symlink() and executable.is_file() and executable.stat().st_mode&0o111 and sha(executable)==v['qemu']['sha256'],'configured QEMU does not match exact private bundle executable')
        mounts.append({'path':str(root),'destination':'/proof/qemu-common/install','readonly':True})
        qemu_bundle={'root':str(root),'manifest':str(mp),'manifest_sha256':sha(mp),'summary':str(sp),'summary_sha256':sha(sp),'build':summary,'bytes':total,'files':len(rows),'non_directory_paths':sorted(n for n,e in expected.items() if e['type']!='directory')}
    return {'driver_override':driver_override,'index':str(index),'index_sha256':sha(index),'producers':producers,'selected':selected,'mounts':mounts,'scripts':scripts,'script_sources':script_sources,'tls':tls,'tls_provenance':tls_provenance,'qemu':v['qemu'],'oci':bundle,'fingerprints':fingerprints,'directory_fingerprints':directory_fingerprints,'link_fingerprints':link_fingerprints,'qemu_bundle':qemu_bundle}

def validate_kd3(raw,inputs):
    plan=private_file(raw);v=json.loads(plan.read_text());require(v.get('version')==1,'KD3 plan version differs')
    root=private_directory(Path(v['root']));manifest=private_file(v['manifest']);expected=json.loads(manifest.read_text())
    actual=file_census(root,8*1024**3)
    require(actual==expected,'prepared KD3 bundle actual complete census differs')
    require(len(actual)<=50000,'KD3 bundle file count exceeds bound')
    bypath={e['path']:e for e in actual}
    def relative(name):
        p=Path(name);require(not p.is_absolute() and '..' not in p.parts and p.as_posix()==name and name in bypath,'KD3 bundle reference escapes actual census');return root/p
    require(set(v['producers'])=={'arm','gnu'},'both actual fixture producers required')
    records={}
    for role,ref in v['producers'].items():
        summary=private_file(ref['summary']);mp=private_file(ref['manifest']);s=json.loads(summary.read_text());m=json.loads(mp.read_text())
        require(s.get('stage_passed') is True and s.get('cleanup') is True and s.get('guest_exit')==0 and s.get('phase')=='kd3-fixture-tools-'+role,'actual fixture producer lacks pass/cleanup')
        require(m.get('passed') is True and m.get('target')==('aarch64-unknown-linux-gnu' if role=='arm' else 'x86_64-unknown-linux-gnu'),'actual fixture target differs')
        names={e['name'] for e in m['files']};require(names==({'moruna-kd3-fixture','_core.so','kd3-fixture-boot','arrow_echo'} if role=='arm' else {'moruna-kd3-fixture','_core.so'}),'actual fixture artifact census differs')
        collection=private_file(summary.parent/'kd3-fixture-artifacts.json');collected=json.loads(collection.read_text())
        require(collected.get('collected') is True and 'error' not in collected and collected.get('actual')==m,'post-cleanup actual fixture collection differs')
        require(all(command.get('exit')==0 for command in m['commands']) and m['commands'],'actual fixture producer commands did not all succeed')
        for item in m['files']:
            artifact=private_file(mp.parent/item['name']);require(artifact.stat().st_size==item['bytes'] and sha(artifact)==item['sha256'],'retained actual fixture artifact changed')
        records[role]=m
        for path in (summary,mp,collection):
            z=path.stat();inputs['fingerprints'].append({'path':str(path),'bytes':z.st_size,'sha256':sha(path),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
    launcher=relative(v['launcher']);row=next(e for e in records['arm']['files'] if e['name']=='kd3-fixture-boot')
    require(launcher.stat().st_size==row['bytes'] and sha(launcher)==row['sha256'] and launcher.stat().st_mode&0o111,'KD3 launcher differs from actual ARM producer')
    cells=relative(v['cells']);matrix=relative(v['matrix']);cellrows=json.loads(cells.read_text());mat=json.loads(matrix.read_text());caseids={c['id'] for c in mat['cases']}
    native=[c for c in cellrows if c['adapter']=='native' and c['phase']=='execute']
    require(len(caseids)==21 and native and len(native)<=64,'actual complete KD3 author matrix required')
    keys=set();runs=set()
    for cell in native:
        key=(cell['case'],cell['mode']);require(cell['case'] in caseids and key not in keys,'duplicate/unknown KD3 execution cell');keys.add(key)
        run=cell['run_id'];require(len(run)==32 and all(c in '0123456789abcdef' for c in run) and run not in runs,'KD3 run must be fresh unique canonical assigned identity');runs.add(run)
        job=json.loads(relative(cell['job']).read_text());command=json.loads(relative(cell['command']).read_text())
        require(job.get('run_id')==run and set(command)=={'program','args','nonce'} and len(command['nonce'])==32 and all(c in '0123456789abcdef' for c in command['nonce']),'KD3 actual job/command identity differs')
        if cell.get('fixture'):
            fixture=json.loads(relative(cell['fixture']).read_text());require(fixture['case']==cell['case'] and fixture['mode']==cell['mode'] and fixture['job']==job,'actual grant fixture disagrees with admitted job')
    required=set()
    for case in mat['cases']:
        modes=('rows','adapter-refusal') if case['runner']=='python-library' else ('rows','unknown-setting') if case['expect']=='rows' else ('rows',)
        required.update((case['id'],mode) for mode in modes)
    require(keys==required,'prepared KD3 execution cells omit required cases/modes')
    for name in ('image_record','image_digest','layer_list'):relative(v[name])
    layerdir=Path(v['layer_dir']);require(not layerdir.is_absolute() and '..' not in layerdir.parts and (root/layerdir).is_dir(),'KD3 derived layer directory differs')
    layer_count=len(list((root/layerdir).glob('stack-*.img')));require(1<=layer_count<=14,'KD3 layer stack must leave room for two real fixture devices')
    require(all((root/layerdir/('stack-'+str(i)+'.img')).relative_to(root).as_posix() in bypath for i in range(1,1+layer_count)),'KD3 canonical stack has gaps')
    for row in actual:
        path=root/row['path'];z=path.stat();inputs['fingerprints'].append({'path':str(path),'bytes':z.st_size,'sha256':row['sha256'],'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
    for path in (plan,manifest):
        z=path.stat();inputs['fingerprints'].append({'path':str(path),'bytes':z.st_size,'sha256':sha(path),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
    if 'linux_results' in v:
        directory=Path(v['linux_results']);require(not directory.is_absolute() and '..' not in directory.parts and (root/directory).is_dir(),'actual Linux result directory differs')
        validator=relative(v['matrix_validator']);source_manifest=private_file(v['matrix_validator_source_manifest']);source_rows=json.loads(source_manifest.read_text());source_row=next((row for row in source_rows if row['path']=='tools/kd3-matrix.py'),None)
        require(source_row is not None and source_row['sha256']==sha(validator) and source_row['bytes']==validator.stat().st_size,'matrix validator differs from exact shipping source census')
        z=source_manifest.stat();inputs['fingerprints'].append({'path':str(source_manifest),'bytes':z.st_size,'sha256':sha(source_manifest),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
    inputs['mounts'].append({'path':str(root),'destination':'/inputs/kd3','readonly':True})
    inputs['kd3']={**v,'plan_sha256':sha(plan),'manifest_sha256':sha(manifest),'native_cells':native}
    inputs['kd3_census']={'root':str(root),'paths':[row['path'] for row in actual]}

GUEST=r'''import hashlib,json,os,pathlib,shutil,subprocess,sys,time
if not __debug__:raise RuntimeError('strict adapter requires assertions')
phase,cpus,token,seconds,archive_sha=sys.argv[1:];cpus=int(cpus);seconds=int(seconds)
archive=pathlib.Path('/tmp/node-helper.tar');assert hashlib.sha256(archive.read_bytes()).hexdigest()==archive_sha,'helper archive changed'
scripts=pathlib.Path('/proof/scripts');scripts.mkdir(mode=0o700)
import tarfile
with tarfile.open(archive) as tar:
 for m in tar:
  p=pathlib.PurePosixPath(m.name);assert not p.is_absolute() and '..' not in p.parts and m.isfile() and m.size<8*1024**2,'unsafe helper member'
  dest=scripts/m.name;dest.parent.mkdir(parents=True,exist_ok=True)
  with dest.open('xb') as f:f.write(tar.extractfile(m).read())
config=json.loads((scripts/'inputs.json').read_text());build=pathlib.Path('/proof/build');evidence=pathlib.Path('/proof/evidence');logs=evidence/('turn-'+token);logs.mkdir(mode=0o700)
deadline=time.monotonic()+seconds;commands=[]
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def command(args,label):
 left=deadline-time.monotonic();assert left>0,'active node deadline'
 log=logs/(label+'.log');process=None;error=None
 try:
  with log.open('xb') as out:
   process=subprocess.Popen([str(x) for x in args],stdout=out,stderr=subprocess.STDOUT,start_new_session=True)
   while process.poll() is None:
    if time.monotonic()>=deadline or log.stat().st_size>32*1024**2 or shutil.disk_usage('/proof/build').free<6*1024**3:
     raise RuntimeError('active deadline/log bound/hard6GiB disk guard')
    time.sleep(.1)
   assert log.stat().st_size<=32*1024**2,'command log bound'
   if process.returncode:raise RuntimeError('actual command failed: '+label)
 except BaseException as failure:
  error=str(failure);raise
 finally:
  if process is not None and process.poll() is None:
   import signal
   os.killpg(process.pid,signal.SIGKILL);process.wait()
  commands.append({'argv':[str(x) for x in args],'exit':process.returncode if process else None,'error':error,'log':str(log),'sha256':sha(log) if log.exists() else None})
  (logs/'commands.json').write_text(json.dumps(commands,indent=2)+'\n')
 return log.read_text()
def cp(source,dest):
 dest.parent.mkdir(parents=True,exist_ok=True);assert not dest.exists(),'never overwrite assembled artifact';shutil.copy2(source,dest)
base=scripts/'kernel';proof=base/'node/tools/proof.py';profile_args=['--qemu',config['qemu']['path'],'--backend',build/'moruna-vhost-vsock','--peer',build/'moruna-kd3-peer','--fixture-emitter',build/'moruna-node-ci-assets','--validator',build/'moruna-node-image','--image-builder',build/'moruna-image-host','--provision',build/'moruna-storage-provision','--kernel-elf',build/'moruna-microkernel.elf','--user-bins',build/'node-user-bins','--image-record',build/'node-linux/image.record','--linux-layers',build/'node-linux/stack.layers','--linux-digest',build/'node-linux/image.digest','--tls-roots',scripts/'tls.roots','--layer-fixture-1',build/'layers/layer-1.img','--layer-fixture-2',build/'layers/layer-2.img','--engine-layer-dir',build/'node-linux']
result={'phase':phase,'cpus':cpus,'passed':False,'commands':commands}
try:
 assert sha(pathlib.Path(config['qemu']['path']))==config['qemu']['sha256'],'actual QEMU differs'
 if phase=='assemble':
  assert not any(build.iterdir()),'assembly must start empty'
  for e in config['selected']['native']:
   source=pathlib.Path('/inputs/native')/e['relative']
   dest=build/'moruna-microkernel.elf' if e['name']=='moruna-microkernel' else build/'node-user-bins'/e['name']
   cp(source,dest)
  renames={'kd3-peer':'moruna-kd3-peer','node-ci-assets':'moruna-node-ci-assets'}
  for e in config['selected']['host']:cp(pathlib.Path('/inputs/host')/e['relative'],build/renames.get(e['name'],e['name']))
  for e in config['selected']['image']:cp(pathlib.Path('/inputs/image')/e['relative'],build/('moruna-image-host' if e['name']=='moruna-image' else e['name']))
  programs=build/'linux/programs'
  for e in config['selected']['personality']:
   n=e['name'];source=pathlib.Path('/inputs/personality')/e['relative']
   if n=='moruna-engine':cp(source,programs/'moruna/bin/moruna-engine')
   elif n in ('transform.py','arrowlite.py'):cp(source,programs/'kd2'/n)
   elif n=='greeting.txt':cp(source,programs/'kc1/greeting.txt')
   elif n in ('hello','threads','refuse','isolate','bench','granted','hog','tap','badrecord','dynamic','arrow_echo'):cp(source,programs/'kc1/bin'/n)
  command([build/'moruna-layer','fixture',build/'layers'],'layer-fixture')
  command([build/'plane-fixture',build/'plane'],'plane-fixture')
  command([build/'kd2-fixture',programs/'kd2'],'kd2-fixture')
  manifest=command(['python3',base/'tools/oci-manifest.py','/inputs/python-oci','linux','amd64'],'oci-manifest').strip();assert manifest==config['oci_manifest'],'actual selected OCI identity differs'
  command([build/'moruna-layer','derive','/inputs/python-oci',manifest,programs,'kd2',build/'linux/image','python3','/kd2/transform.py'],'derive')
  command([build/'moruna-layer','stack',build/'linux/image','kd2',build/'linux',3],'generic-stack')
  command([build/'moruna-layer','stack',build/'linux/image','kd2',build/'node-linux',2],'node-stack')
 elif phase=='refresh-engine':
  import re
  transition=config['engine_refresh'];assert transition,'explicit actual engine-client transition required'
  before={e['path']:e for e in config['refresh_expected']}
  def verify_file(path,relative):
   e=before[relative];assert path.is_file() and not path.is_symlink() and path.stat().st_size==e['bytes'] and sha(path)==e['sha256'],'old clone changed before controlled replacement'
  for n,e in before.items():verify_file(build/n,n)
  assert sha(build/transition['path'])==transition['sha256'],'actual new engine changed'
  stage=build/'.engine-client-refresh';stage.mkdir(mode=0o700)
  manifest=command(['python3',base/'tools/oci-manifest.py','/inputs/python-oci','linux','amd64'],'refresh-oci-manifest').strip();assert manifest==config['oci_manifest'],'actual selected OCI identity differs'
  command([build/'moruna-layer','derive','/inputs/python-oci',manifest,build/'linux/programs','kd2',stage/'image','python3','/kd2/transform.py'],'refresh-derive')
  command([build/'moruna-layer','stack',stage/'image','kd2',stage/'generic',3],'refresh-generic-stack')
  command([build/'moruna-layer','stack',stage/'image','kd2',stage/'node',2],'refresh-node-stack')
  assert (stage/'generic/image.digest').read_bytes()==(stage/'node/image.digest').read_bytes(),'canonical derived image identities differ'
  assert (stage/'generic/image.record').read_bytes()==(stage/'node/image.record').read_bytes(),'canonical derived image records differ'
  def stack_name(n):return n in ('stack.layers','image.record','image.digest') or re.fullmatch(r'stack-[1-9][0-9]*\.img',n) is not None
  # Delete only exact previously verified derived files in OUR NEW clone;
  # old accepted source/output and every failure remain untouched.
  image=build/'linux/image'
  for n in sorted((n for n in before if n.startswith('linux/image/')),reverse=True):
   verify_file(build/n,n);(build/n).unlink()
  for parent,dirs,files in os.walk(image,topdown=False):
   assert not files,'unexpected derived image file';pathlib.Path(parent).rmdir()
  (stage/'image').rename(image)
  for old,new in [('linux','generic'),('node-linux','node')]:
   folder=build/old;generated=stage/new
   entries=list(generated.iterdir());assert entries and len(entries)<=20,'canonical stack output bound'
   assert all(p.is_file() and not p.is_symlink() and stack_name(p.name) for p in entries),'unexpected canonical stack output'
   for n in before:
    if pathlib.PurePosixPath(n).parent.as_posix()==old and stack_name(pathlib.PurePosixPath(n).name):verify_file(build/n,n);(build/n).unlink()
   for p in entries:p.rename(folder/p.name)
   generated.rmdir()
  stage.rmdir()
  result['engine_client_refreshed']=True
 elif phase=='profile':
  assert cpus in (1,2,4),'exact CPU profile required'
  parent=evidence/('cpu'+str(cpus));parent.mkdir(mode=0o700,exist_ok=True)
  for case in range(1,6):
   out=parent/('case'+str(case))
   if out.exists():
    assert case in config['verified_cases'],'existing case must have an exact prior accepted census'
    continue
   command(['python3',proof,*profile_args,'--sdk-proof',build/'kd3-native-proof','--cpus',cpus,'--case',case,'--out',out,'--seconds',180],'case'+str(case))
  if all((evidence/('cpu'+str(cpu))/('case'+str(case))/'result.json').is_file() for cpu in (1,2,4) for case in range(1,6)):
   command(['python3',base/'tools/check-node-fixtures.py',evidence],'matrix15')
   result['matrix15_passed']=True
 elif phase=='kd3':
  k=config['kd3'];bundle=pathlib.Path('/inputs/kd3');root=evidence/'kd3';root.mkdir(mode=0o700,exist_ok=True)
  cellroot=root/'cells';cellroot.mkdir(mode=0o700,exist_ok=True);results=root/'results';results.mkdir(mode=0o700,exist_ok=True)
  matrix=json.loads((bundle/k['matrix']).read_text());cases={c['id']:c for c in matrix['cases']};expected=sorted(matrix['rows'])
  kd3args=list(profile_args)
  for option,name in [('--image-record','image_record'),('--linux-layers','layer_list'),('--linux-digest','image_digest'),('--engine-layer-dir','layer_dir')]:
   kd3args[kd3args.index(option)+1]=bundle/k[name]
  executed=0
  for index,cell in enumerate(k['native_cells']):
   out=cellroot/str(index);case=cases[cell['case']]
   if out.exists():
    prior=json.loads((out/'result.json').read_text()) if (out/'result.json').is_file() else {}
    assert prior.get('passed') is True and prior.get('cleanup') is True and (out/'collected.json').is_file(),'failed/partial native KD3 cell retained; never overwrite/relabel'
    continue
   nonce=json.loads((bundle/cell['command']).read_text())['nonce']
   args=['python3',proof,*kd3args,'--cpus',1,'--case',1,'--out',out,'--seconds',180,
         '--kd3-conformance-proof',bundle/cell['command'],'--kd3-job',bundle/cell['job'],
         '--fixture-boot',bundle/k['launcher'],'--fixture-cell',cell['case']]
   if cell.get('fixture'):args+=['--kd3-case',bundle/cell['fixture']]
   command(args,'kd3-'+str(index))
   actual=json.loads((out/'result.json').read_text());receipt=json.loads((out/'fixture-result.json').read_text())
   assert actual.get('passed') is True and actual.get('cleanup') is True and receipt.get('cleanup') is True,'actual native fixture/cleanup failed'
   assert receipt['case']==cell['case'] and receipt['run']==cell['run_id'] and receipt['nonce']==nonce,'actual native fixture identity differs'
   payload=receipt['result'];assert payload['run_id']==cell['run_id'],'actual assigned Guest run differs'
   rows=payload['receipts'];want={('execute',cell['mode'])}
   if case['runner']=='document' and case['role']=='sink' and case['expect']=='rows' and cell['mode']=='rows':want.add(('readback','rows'))
   assert len(rows)==len(want) and {(r['phase'],r['mode']) for r in rows}==want,'actual native phase receipts incomplete/duplicate'
   execute=next(r for r in rows if r['phase']=='execute')
   for r in rows:
    assert r['case']==cell['case'] and r['adapter']=='native' and r['run_id']==cell['run_id'],'actual receipt case/adapter/run mismatch'
    refused=r['mode'] in ('unknown-setting','adapter-refusal') or case['expect']=='strict-import-refusal'
    if refused:assert r['outcome']=='refused' and r.get('diagnostic'),'missing real expected refusal'
    else:
     assert r['outcome']=='rows' and sorted(r['rows'])==expected and r['finish']==1 and r['committed']==len(expected),'actual native rows/commit/finish differ'
     if case['runner']=='document':assert isinstance(r.get('outside_after'),list) and not any(r['outside_after']),'actual native task reservation remains'
    if r['phase']=='readback':assert execute.get('files') and r['producer']==execute,'readback not bound to exact actual sink ACK'
    name='.'.join([r['case'],r['adapter'],r['phase'],r['mode']])+'.json'
    with (results/name).open('x') as target:json.dump(r,target,indent=2);target.write('\n')
   with (out/'collected.json').open('x') as collected:json.dump({'run_id':cell['run_id'],'case':cell['case'],'mode':cell['mode'],'receipt_sha256':sha(out/'fixture-result.json')},collected)
   executed+=1
   if executed>=config['kd3_count']:break
  result['kd3_executed_this_turn']=executed
  complete=all((cellroot/str(i)/'result.json').is_file() and json.loads((cellroot/str(i)/'result.json').read_text()).get('passed') is True and (cellroot/str(i)/'collected.json').is_file() for i in range(len(k['native_cells'])))
  result['kd3_native_complete']=complete
  result['kd3_native_receipts']=len(list(results.glob('*.json')))
  if complete and k.get('linux_results'):
   combined=root/'matrix-inputs';assert not combined.exists(),'final matrix evidence already exists, never overwrite'
   combined.mkdir(mode=0o700)
   for directory in (results,bundle/k['linux_results']):
    for source in sorted(directory.glob('*.json')):
     assert source.is_file() and not source.is_symlink(),'matrix input must be real regular receipt'
     record=json.loads(source.read_text());name='.'.join(str(record[field]) for field in ('case','adapter','phase','mode'))+'.json'
     assert '/' not in name and '..' not in name and not (combined/name).exists(),'matrix actual receipt duplicate/unsafe identity'
     shutil.copy2(source,combined/name)
   command(['python3',bundle/k['matrix_validator'],bundle/k['matrix'],combined],'kd3-matrix92')
   result['kd3_matrix_passed']=True
 elif phase=='io':
  command(['python3',base/'node/tools/io-proof.py','--out',evidence/'io','--net-proof',base/'tools/net-proof.py','--cpus',1,'--seconds',180,'--',*profile_args],'io-two-boots')
 else:raise RuntimeError('unknown canonical phase')
 result['passed']=True
finally:
 (logs/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print('MORUNA_NODE_ACCEPTANCE_DONE '+token+' '+phase,flush=True)
'''

def input_fingerprints_unchanged(inputs):
    for e in inputs['fingerprints']:
        p=private_file(e['path']);z=p.stat();require((z.st_dev,z.st_ino,z.st_size,z.st_mtime_ns,z.st_uid,stat.S_IMODE(z.st_mode))==(e['device'],e['inode'],e['bytes'],e['mtime_ns'],e['uid'],e['mode']) and sha(p)==e['sha256'],'original producer/input changed')
    for e in inputs.get('directory_fingerprints',[]):
        p=private_directory(Path(e['path']));z=p.stat();require((z.st_dev,z.st_ino,z.st_uid)==(e['device'],e['inode'],e['uid']),'private QEMU root identity changed')
    for e in inputs.get('link_fingerprints',[]):
        p=Path(e['path']);z=p.lstat();require(stat.S_ISLNK(z.st_mode) and (z.st_dev,z.st_ino,z.st_uid)==(e['device'],e['inode'],e['uid']) and os.readlink(p)==e['target'],'private QEMU link identity changed')
    if inputs.get('kd3_census'):
        census=inputs['kd3_census'];root=Path(census['root']);actual=set()
        for base,dirs,files in os.walk(root,followlinks=False):
            for name in dirs:require(not (Path(base)/name).is_symlink(),'KD3 input directory gained symlink')
            for name in files:
                path=Path(base)/name;require(stat.S_ISREG(path.lstat().st_mode),'KD3 input gained nonregular file');actual.add(path.relative_to(root).as_posix())
        require(actual==set(census['paths']),'KD3 input full file census changed')
    bundle=inputs.get('qemu_bundle')
    if bundle:
        root=Path(bundle['root']);actual=set()
        for base,dirs,files in os.walk(root,followlinks=False):
            for name in dirs+files:
                p=Path(base)/name
                if p==Path(bundle['manifest']):continue
                if not p.is_dir() or p.is_symlink():actual.add(p.relative_to(root).as_posix())
        require(actual==set(bundle['non_directory_paths']),'private QEMU file census changed')

def driver_reuse_plan(old,inputs,j,jp):
    base_output=ROOT/'node-acceptance-controller-admission-output-3e682418af86'
    buffered_output=ROOT/'node-acceptance-buffered-driver-output-298df6800d5f'
    require(old in (base_output,buffered_output),'driver-only reuse requires exact known current assembled build')
    require(old==(buffered_output if (inputs.get('driver_override') or {}).get('sink_ack_tests') else base_output),'sink criterion fork must start from actual298 buffered build')
    base_jp=private_file(base_output/'journal.json');base_j=json.loads(base_jp.read_text());base_stat=base_output.stat()
    require(base_j.get('assembled') is True and not base_j.get('refresh_pending') and not(base_output/'busy').exists() and (base_stat.st_dev,base_stat.st_ino,base_stat.st_uid)==(base_j['device'],base_j['inode'],base_j['uid']),'actual1800 base identity/readiness differs')
    if old==buffered_output:
        prior_clone=j['assembly_reuse'];prior_transition=prior_clone['driver_transition']
        require(prior_clone.get('completed') is True and Path(prior_clone['source']['old_output'])==base_output and prior_clone['source']['old_journal_sha256']==sha(base_jp),'buffered fork base lineage differs')
        require(prior_transition['sha256']=='f65f04a743f2bb67c25fc1fd73623960240288fce8b1d45c3b50698c26f18159' and Path(prior_transition['summary'])==ROOT/'moruna-proof-501-298df6800d5f/summary.json','buffered fork actual298 driver differs')
        require(prior_transition['summary_sha256']==sha(private_file(ROOT/'moruna-proof-501-298df6800d5f/summary.json')) and prior_transition['manifest_sha256']==sha(private_file(ROOT/'moruna-proof-501-298df6800d5f/witness-proof/boot-host-tools.json')) and prior_transition['source_provenance_sha256']==sha(private_file(ROOT/'combined-inputs-_chzmbrc/provenance.json')),'buffered fork actual298 custody changed')
        before={e['path']:e for e in base_j['build_census']};after={e['path']:e for e in j['build_census']}
        require(set(before)==set(after) and {n for n in before if before[n]!=after[n]}=={'kd3-native-proof'} and after['kd3-native-proof']['sha256']==prior_transition['sha256'],'buffered fork changed image or other build bytes')
        require(file_census(base_output/'build',4*1024**3)==base_j['build_census'],'actual1800 base build changed')
    require(inputs.get('driver_override') is not None and not j.get('refresh_pending'),'actual driver override/finished image required')
    refresh=private_file(ROOT/'moruna-proof-501-1800a7ff19fc/summary.json');r=json.loads(refresh.read_text())
    require(r.get('phase')=='refresh-engine' and r.get('phase_passed') is True and r.get('cleanup') is True and r.get('guest_exit')==0 and r.get('input_index_sha256')==base_j['inputs_sha256'],'latest actual image refresh not proved')
    require(any(Path(t['summary'])==refresh and t.get('phase_passed') is True for t in base_j['turns']),'latest refresh not owned by current build')
    for turn in j['turns']:require(json.loads(private_file(turn['summary']).read_text()).get('cleanup') is True,'prior current output owner cleanup missing')
    original=private_directory(ROOT/'node-acceptance-output-03676b92a6e3');original_jp=private_file(original/'journal.json');original_j=json.loads(original_jp.read_text());lineage=base_j['assembly_reuse']
    require(lineage.get('completed') is True and Path(lineage['source']['old_output'])==original and sha(original_jp)==lineage['source']['old_journal_sha256'],'original03676 assembly lineage differs')
    assembly=private_file(lineage['source']['old_assembly_summary']);aresult=json.loads(assembly.read_text())
    require(sha(assembly)==lineage['source']['old_assembly_summary_sha256'] and aresult.get('phase')=='assemble' and aresult.get('phase_passed') is True and aresult.get('cleanup') is True and aresult.get('guest_exit')==0,'original actual assembly proof differs')
    require(file_census(original/'build',4*1024**3)==original_j['build_census'],'original assembly census changed')
    old_index=private_file((ROOT/'moruna-proof-501-ae513a1de7da/input-index.json') if old==buffered_output else refresh.parent/'input-index.json');require(sha(old_index)==j['inputs_sha256'],'current assembled index binding differs')
    a=json.loads(old_index.read_text());b=json.loads(Path(inputs['index']).read_text());require(('driver_override' in a)==(old==buffered_output) and 'driver_override' in b,'driver override index boundary differs');a.pop('driver_override',None);b.pop('driver_override');require(a==b,'driver-only fork changes other producer/script/OCI/QEMU input')
    old_inputs=validate_index(old_index)
    for role in ROLES:
        before={e['name']:e for e in old_inputs['selected'][role]};after={e['name']:e for e in inputs['selected'][role]};require(set(before)==set(after),'driver fork artifact set differs')
        require({n for n in before if before[n]!=after[n]}==({'kd3-native-proof'} if role=='host' else set()),'driver fork changes other selected artifact')
    require(file_census(old/'build',4*1024**3)==j['build_census'],'latest actual assembled census changed')
    for path in (jp,base_jp,original_jp,assembly,refresh,old_index):
        z=path.stat();inputs['fingerprints'].append({'path':str(path),'bytes':z.st_size,'sha256':sha(path),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
    old_driver=next(e for e in old_inputs['selected']['host'] if e['name']=='kd3-native-proof')
    if old==buffered_output:require(old_driver['sha256']=='f65f04a743f2bb67c25fc1fd73623960240288fce8b1d45c3b50698c26f18159','buffered selected driver differs from actual298')
    transition={**inputs['driver_override'],'old_sha256':old_driver['sha256'],'old_bytes':old_driver['bytes'],'prior_driver_override':old_inputs.get('driver_override'),'path':'kd3-native-proof','actual_refresh_summary':str(refresh),'actual_refresh_summary_sha256':sha(refresh)}
    expected=[dict(e) for e in j['build_census']];rows=[e for e in expected if e['path']=='kd3-native-proof'];require(len(rows)==1 and rows[0]['sha256']==transition['old_sha256'] and rows[0]['bytes']==transition['old_bytes'],'assembled driver differs from512base')
    rows[0].update(bytes=transition['bytes'],sha256=transition['sha256'],mode=transition['source_mode'])
    return {'old_output':str(old),'old_journal':str(jp),'old_journal_sha256':sha(jp),'old_index_sha256':sha(old_index),'old_assembly_summary':str(assembly),'old_assembly_summary_sha256':sha(assembly),'build_census':j['build_census'],'expected_build_census':expected,'supervisor_tls_transition':None,'engine_client_transition':None,'driver_transition':transition}

def reuse_plan(old_raw,inputs,supervisor_tls=False,engine_client=False,driver_only=False):
    require(inputs.get('qemu_bundle') is not None,'assembly reuse requires an actually verified corrected QEMU bundle')
    old=private_directory(Path(old_raw).absolute());jp=private_file(old/'journal.json');j=json.loads(jp.read_text());z=old.stat()
    require(j.get('version')==1 and j.get('assembled') is True and (z.st_dev,z.st_ino,z.st_uid)==(j['device'],j['inode'],j['uid']),'old assembly journal identity differs')
    require(not (old/'busy').exists(),'old output is still reserved')
    if driver_only:
        require(not supervisor_tls and not engine_client,'driver reuse cannot perform other transitions')
        return driver_reuse_plan(old,inputs,j,jp)
    require(inputs.get('driver_override') is None,'driver override requires explicit driver-only fork')
    assemblies=[]
    for turn in j.get('turns',[]):
        p=private_file(turn['summary']);summary=json.loads(p.read_text());require(summary.get('cleanup') is True,'prior output owner lacks actual cleanup')
        if turn['phase']=='assemble' and turn.get('phase_passed') is True:
            require(summary.get('phase_passed') is True and summary.get('guest_exit')==0,'old actual assembly result differs');assemblies.append(p)
    require(len(assemblies)==1,'one actual successful assembly producer required')
    old_index=private_file(assemblies[0].parent/'input-index.json');require(sha(old_index)==j['inputs_sha256'],'old input index binding differs')
    a=json.loads(old_index.read_text());b=json.loads(Path(inputs['index']).read_text())
    transition=None;engine_transition=None;expected_build=j['build_census']
    if supervisor_tls:
        # Exactly the accepted bootstrap-console correction, never an arbitrary
        # artifact replacement or a broad producer/source compatibility switch.
        require(old==ROOT/'node-acceptance-output-03676b92a6e3','supervisor/TLS transition requires the original actual assembly output')
        old_native=a['producers']['native'];old_manifest=private_file(old_native['manifest']);assembly=json.loads(assemblies[0].read_text())
        require(sha(old_manifest)==assembly['producer_provenance']['native']['manifest_sha256'],'old accepted native manifest changed')
        old_rows=json.loads(old_manifest.read_text());new_rows=inputs['selected']['native']
        before={(e['target'],e['name']):e for e in old_rows};after={(Path(e['relative']).parent.as_posix(),e['name']):e for e in new_rows}
        require(len(before)==len(after)==52 and set(before)==set(after),'supervisor transition requires the same actual52 native targets')
        changed={key for key in before if (before[key]['bytes'],before[key]['sha256'])!=(after[key]['bytes'],after[key]['sha256'])}
        key=('x86_64-moruna-user','node-supervisor')
        require(changed=={key},'only node-supervisor may change; all other51 native bytes must match')
        old_root=private_directory(Path(old_native['root']))
        for (target,name),row in before.items():
            p=private_file(old_root/target/name);require(p.stat().st_size==row['bytes'] and sha(p)==row['sha256'],'old native actual artifact changed')
        old_row=before[key];new_row=after[key];source=private_file(Path(inputs['producers']['native']['root'])/new_row['relative']);z=source.stat()
        require(z.st_mode&0o111,'new actual node-supervisor is not executable')
        expected_build=[dict(e) for e in j['build_census']];matches=[e for e in expected_build if e['path']=='node-user-bins/node-supervisor'];require(len(matches)==1,'one actual assembled supervisor required')
        require((matches[0]['bytes'],matches[0]['sha256'])==(old_row['bytes'],old_row['sha256']),'assembled supervisor does not match old actual producer')
        matches[0].update(bytes=new_row['bytes'],sha256=new_row['sha256'],mode=stat.S_IMODE(z.st_mode))
        provenance=inputs.get('tls_provenance');require(provenance is not None,'controlled TLS transition requires actual net-proof preparation provenance')
        tls=json.loads(provenance.read_text());tls_entries={e['path']:e for e in tls['files']}
        require(tls['source_sha256']==sha(inputs['scripts']['kernel/tools/net-proof.py']) and tls['kernel_sha256']==before[('x86_64-unknown-none','moruna-microkernel')]['sha256'],'controlled TLS source/kernel provenance differs')
        require(Path(tls['roots'])==inputs['tls'] and tls['roots_sha256']==sha(inputs['tls'])==tls_entries['ke2/roots']['sha256'] and inputs['tls'].stat().st_size==tls_entries['ke2/roots']['bytes'],'actual controlled TLS roots differ')
        require('ke2/root.der' in tls_entries and 'ke2/ca.pem' in tls_entries and 'ke2/peer.pem' in tls_entries,'actual controlled CA preparation artifacts absent')
        for name in ('ke2/root.der','ke2/ca.pem','ke2/peer.pem'):
            p=private_file(provenance.parent/name);require(p.stat().st_size==tls_entries[name]['bytes'] and sha(p)==tls_entries[name]['sha256'],'controlled TLS artifact changed')
        transition={'path':'node-user-bins/node-supervisor','source':str(source),'bytes':new_row['bytes'],'sha256':new_row['sha256'],'source_mode':stat.S_IMODE(z.st_mode),'old_sha256':old_row['sha256'],'old_native_manifest':str(old_manifest),'old_native_manifest_sha256':sha(old_manifest),'new_native_manifest_sha256':inputs['producers']['native']['manifest_sha256'],'new_native_summary_sha256':inputs['producers']['native']['summary_sha256'],'tls_roots':str(inputs['tls']),'tls_sha256':sha(inputs['tls']),'tls_provenance':str(provenance),'tls_provenance_sha256':sha(provenance),'unchanged_native_artifacts':51}
        for document in (a,b):document['producers'].pop('native');document.pop('tls_roots')
    if engine_client:
        require(supervisor_tls,'engine-client correction requires the explicit original supervisor/TLS transition')
        old_ref=a['producers']['personality'];old_manifest=private_file(old_ref['manifest']);assembly=json.loads(assemblies[0].read_text())
        require(sha(old_manifest)==assembly['producer_provenance']['personality']['manifest_sha256'],'old personality manifest changed')
        old_progress=json.loads(old_manifest.read_text());old_rows={e['name']:e for e in old_progress['artifacts']};new_rows={e['name']:e for e in inputs['selected']['personality']}
        require(set(old_rows)==set(new_rows),'engine-only producer artifact names changed')
        payload={'moruna-engine','arrow_echo','hello','threads','refuse','isolate','bench','granted','hog','tap','badrecord','dynamic','transform.py','arrowlite.py','greeting.txt'}
        require(payload<=set(old_rows),'actual runnable/payload set incomplete')
        changed={n for n in payload if (old_rows[n]['bytes'],old_rows[n]['sha256'])!=(new_rows[n]['bytes'],new_rows[n]['sha256'])}
        require(changed=={'moruna-engine'},'C11 runnable/data payload changed; only the actual Rust engine may differ')
        progress_path=private_file(inputs['producers']['personality']['manifest_path']);progress=json.loads(progress_path.read_text())
        require(progress.get('layers_client_tests',{}).get('passed') is True,'actual shipping-client protocol tests are absent')
        tests=[e for e in progress['commands'] if e['name']=='layers-client-tests'];require(len(tests)==1 and tests[0]['exit']==0 and tests[0]['log']=='layers-client-tests.log','one actual successful protocol test command required')
        log=private_file(Path(inputs['producers']['personality']['root'])/'layers-client-tests.log');require(log.stat().st_size<=8*1024**2 and sha(log)==tests[0]['sha256'],'actual protocol test log hash differs')
        counts=re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log.read_text(),re.MULTILINE)
        require(counts==[('5','0','0')],'actual protocol result must be exactly five passed, zero failed/ignored')
        z=log.stat();inputs['fingerprints'].append({'path':str(log),'bytes':z.st_size,'sha256':sha(log),'device':z.st_dev,'inode':z.st_ino,'mtime_ns':z.st_mtime_ns,'uid':z.st_uid,'mode':stat.S_IMODE(z.st_mode)})
        row=new_rows['moruna-engine'];source=private_file(Path(inputs['producers']['personality']['root'])/row['relative']);z=source.stat();require(z.st_mode&0o111,'new actual engine is not executable')
        matches=[e for e in expected_build if e['path']=='linux/programs/moruna/bin/moruna-engine'];require(len(matches)==1 and (matches[0]['bytes'],matches[0]['sha256'])==(old_rows['moruna-engine']['bytes'],old_rows['moruna-engine']['sha256']),'assembled engine does not match original producer')
        matches[0].update(bytes=row['bytes'],sha256=row['sha256'],mode=stat.S_IMODE(z.st_mode))
        engine_transition={'path':'linux/programs/moruna/bin/moruna-engine','source':str(source),'bytes':row['bytes'],'sha256':row['sha256'],'source_mode':stat.S_IMODE(z.st_mode),'old_sha256':old_rows['moruna-engine']['sha256'],'old_personality_manifest_sha256':sha(old_manifest),'new_personality_manifest_sha256':sha(progress_path),'new_personality_summary_sha256':inputs['producers']['personality']['summary_sha256'],'protocol_log':str(log),'protocol_log_sha256':sha(log),'protocol_result':{'passed':5,'failed':0,'ignored':0},'unchanged_payloads':sorted(payload-{'moruna-engine'})}
        for document in (a,b):document['producers'].pop('personality')
    for document in (a,b):
        document.pop('qemu',None);document.pop('qemu_bundle',None)
    require(a==b,'assembly reuse changes inputs beyond the explicit QEMU/supervisor/TLS boundary')
    require(file_census(old/'build',4*1024**3)==j['build_census'],'old accepted assembly bytes/census changed')
    return {'old_output':str(old),'old_journal':str(jp),'old_journal_sha256':sha(jp),'old_index_sha256':sha(old_index),'old_assembly_summary':str(assemblies[0]),'old_assembly_summary_sha256':sha(assemblies[0]),'build_census':j['build_census'],'expected_build_census':expected_build,'supervisor_tls_transition':transition,'engine_client_transition':engine_transition}

def clone_assembly(o,inputs,plan,lease):
    import ctypes
    require(sys.platform=='darwin','independent APFS clone preparation requires macOS; no copy/hardlink fallback')
    out=Path(o.output);source=Path(plan['old_output'])/'build';require(not out.exists() and not out.is_symlink(),'reuse destination must be new')
    require(sha(Path(plan['old_journal']))==plan['old_journal_sha256'] and file_census(source,4*1024**3)==plan['build_census'],'reuse source changed')
    input_fingerprints_unchanged(inputs);out.mkdir(mode=0o700);dest=out/'build';dest.mkdir(mode=0o700);(out/'evidence').mkdir(mode=0o700)
    lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib',use_errno=True);lib.clonefile.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_int];lib.clonefile.restype=ctypes.c_int
    record={'mode':'host-only APFS independent-inode assembly reuse','source':plan,'inputs_sha256':inputs['index_sha256'],'clone_flags':10,'completed':False,'files':[]};directories=[];deadline=time.monotonic()+min(o.seconds,1050)
    try:
        for base,dirs,files in os.walk(source,followlinks=False):
            require(shutil.disk_usage(ROOT).free>=6*1024**3,'hard6GiB reuse disk guard')
            require(time.monotonic()<deadline and lease.fingerprint==(lease.lock.stat().st_dev,lease.lock.stat().st_ino) and (lease.lock/'owner').read_text().strip()==lease.owner,'reuse deadline/lease differs')
            parent=Path(base);relative=parent.relative_to(source);target=dest/relative;z=parent.stat();directories.append((target,z))
            for name in dirs:
                child=parent/name;require(not child.is_symlink(),'reuse source directory link');(target/name).mkdir(mode=0o700)
            for name in files:
                p=private_file(parent/name);q=target/name;before=p.stat();require(before.st_nlink==1,'reuse source hardlink')
                if lib.clonefile(os.fsencode(p),os.fsencode(q),0x8|0x2):raise OSError(ctypes.get_errno(),'clonefile refused; no copy fallback',str(p))
                os.chmod(q,stat.S_IMODE(before.st_mode));os.utime(q,ns=(before.st_atime_ns,before.st_mtime_ns));after=q.stat()
                require(after.st_ino!=before.st_ino and after.st_uid==before.st_uid and after.st_nlink==1 and after.st_mtime_ns==before.st_mtime_ns and sha(q)==sha(p),'independent cloned metadata/bytes differ')
                record['files'].append({'path':q.relative_to(dest).as_posix(),'source_device':before.st_dev,'source_inode':before.st_ino,'device':after.st_dev,'inode':after.st_ino,'uid':after.st_uid,'mode':stat.S_IMODE(after.st_mode),'mtime_ns':after.st_mtime_ns,'sha256':sha(q)})
        for kind in ('supervisor_tls_transition','engine_client_transition','driver_transition'):
            transition=plan.get(kind)
            if not transition:continue
            source_executable=private_file(transition['source']);before=source_executable.stat();require(sha(source_executable)==transition['sha256'] and before.st_size==transition['bytes'],'verified replacement executable changed')
            destination=dest/transition['path'];temporary=destination.with_name('.'+destination.name+'-transition')
            if lib.clonefile(os.fsencode(source_executable),os.fsencode(temporary),0x8|0x2):raise OSError(ctypes.get_errno(),'replacement executable clone refused',str(source_executable))
            os.chmod(temporary,transition['source_mode']);os.utime(temporary,ns=(before.st_atime_ns,before.st_mtime_ns));after=temporary.stat()
            require(after.st_ino!=before.st_ino and after.st_nlink==1 and after.st_uid==before.st_uid and sha(temporary)==transition['sha256'],'replacement clone identity/bytes differ')
            os.replace(temporary,destination) # ONLY this new clone, never its original producer/output.
            record[kind]={**transition,'source_inode':before.st_ino,'inode':after.st_ino,'mtime_ns':after.st_mtime_ns}
        for target,z in reversed(directories):os.chmod(target,stat.S_IMODE(z.st_mode));os.utime(target,ns=(z.st_atime_ns,z.st_mtime_ns))
        require(file_census(dest,4*1024**3)==plan['expected_build_census'] and file_census(source,4*1024**3)==plan['build_census'] and sha(Path(plan['old_journal']))==plan['old_journal_sha256'],'reuse source/new census changed')
        input_fingerprints_unchanged(inputs);z=out.stat();record['completed']=True
        journal={'version':1,'device':z.st_dev,'inode':z.st_ino,'uid':z.st_uid,'inputs_sha256':inputs['index_sha256'],'assembled':plan.get('engine_client_transition') is None,'refresh_pending':plan.get('engine_client_transition') is not None,'engine_refresh':plan.get('engine_client_transition'),'cases':{},'build_census':plan['expected_build_census'],'assembly_reuse':record,'turns':[]}
        atomic_json(out/'journal.json',journal)
    finally:
        atomic_json(out/'reuse-result.json',record)
    print(json.dumps({'host_clone_completed':True,'output':str(out),'files':len(record['files']),'no_container_commands':True}))

class NodeAcceptanceRun(Run):
    def __init__(self,o,inputs,journal):
        super().__init__(o);self.memory=6;self.inputs=inputs;self.journal=journal;self.output=Path(o.output);self.busy=False;self.mounts=[]
        self.attachments=[*inputs['mounts'],{'path':str(self.output/'build'),'destination':'/proof/build','readonly':o.phase not in ('assemble','refresh-engine')},{'path':str(self.output/'evidence'),'destination':'/proof/evidence','readonly':False}]
    def guard(self):
        super().guard();require(shutil.disk_usage(ROOT).free>=6*1024**3,'hard6GiB free disk guard');require(time.monotonic()<self.active_deadline,'active1050s bound')
    def owned(self,cleanup=False):
        # Input corruption must not prevent teardown of the independently nonce-owned VM.
        if not cleanup and self.inputs.get('qemu_bundle'):input_fingerprints_unchanged(self.inputs)
        r=super().owned(cleanup)
        if r:
            validate_inspect(r['configuration'],self.attachments);c=r['configuration'];require(c['resources']['memoryInBytes']==6*1024**3 and c['image']['descriptor']['digest']==self.pins['index'],'node VM resources/image differ')
        return r
    def unchanged(self):
        input_fingerprints_unchanged(self.inputs)
    def archive(self):
        p=self.evidence/'helper-inputs.tar';config={'selected':self.inputs['selected'],'qemu':self.inputs['qemu'],'oci_manifest':self.inputs['oci']['selected_manifest'],'verified_cases':[],'engine_refresh':self.journal.get('engine_refresh'),'refresh_expected':self.journal.get('build_census')}
        if self.options.phase=='kd3':config.update(kd3=self.inputs['kd3'],kd3_count=self.options.kd3_count)
        if self.options.phase=='profile':config['verified_cases']=[int(c) for c in self.journal.get('cases',{}).get(str(self.options.cpus),{})]
        items={n:p.read_bytes() for n,p in self.inputs['scripts'].items()};items.update({'inputs.json':json.dumps(config).encode(),'tls.roots':self.inputs['tls'].read_bytes(),'node-acceptance.py':Path(__file__).read_bytes()})
        with tarfile.open(p,'w') as tar:
            for n,data in sorted(items.items()):
                require(len(data)<8*1024**2,'helper size bound');m=tarfile.TarInfo(n);m.size=len(data);m.mode=0o400;tar.addfile(m,io.BytesIO(data))
        return p
    def execute(self):
        self.evidence.mkdir(mode=0o700);summary={'name':self.name,'nonce':self.token,'phase':self.options.phase,'cpus':2,'memory_gib':6,'cleanup':False,'input_index_sha256':self.inputs['index_sha256'],'producer_provenance':self.inputs['producers'],'driver_override':self.inputs.get('driver_override'),'script_sources':self.inputs['script_sources'],'scripts':{n:sha(ROOT/n) for n in ('node-acceptance.py','run.py','attachments.py')},'readonly_inputs':self.inputs['mounts'],'qemu_bundle':self.inputs.get('qemu_bundle')};failure=None
        self.active_deadline=min(time.monotonic()+min(self.options.seconds,1050),self.deadline-self.options.seconds+1050)
        try:
            require(shutil.disk_usage(ROOT).free>=8*1024**3,'normal8GiB admission headroom required');self.unchanged();self.cached_prerequisites();self.active_deadline=min(self.active_deadline,self.deadline-40)
            (self.output/'busy').mkdir(mode=0o700);self.busy=True;(self.output/'busy/owner').write_text(self.owner+'\n')
            source=self.archive();summary['helper_archive_sha256']=sha(source);summary['guest_program_sha256']=hashlib.sha256(GUEST.encode()).hexdigest();(self.evidence/'guest-command.py').write_text(GUEST)
            shutil.copy2(self.inputs['index'],self.evidence/'input-index.json')
            if self.journal.get('engine_refresh'):
                shutil.copy2(self.journal['engine_refresh']['protocol_log'],self.evidence/'layers-client-tests.log')
            for role,reference in json.loads(Path(self.inputs['index']).read_text())['producers'].items():
                for kind in ('summary','manifest'):shutil.copy2(reference[kind],self.evidence/(role+'-'+kind+'.json'))
            self.call(['create','--name',self.name,'--label',LABEL+'='+self.token,'--cpus','2','--memory','6G','--platform','linux/arm64','--init-image',self.pins['init'],'--kernel',self.pins['kernel_path'],'--no-dns',*flags(self.attachments),'--entrypoint','/bin/bash',self.pins['image'],*self.arguments],seconds=45)
            self.owned();self.call(['start',self.name]);self.owned();self.call(['copy',str(source),self.name+':/tmp/node-helper.tar'],seconds=30)
            code,text=self.call(['exec',self.name,'python3','-c',GUEST,self.options.phase,str(self.options.cpus or 0),self.token,str(max(1,int(self.active_deadline-time.monotonic())-10)),sha(source)],seconds=max(1,int(self.active_deadline-time.monotonic())),check=False);summary['guest_exit']=code
            self.unchanged();require(code==0 and 'MORUNA_NODE_ACCEPTANCE_DONE '+self.token+' '+self.options.phase in text,'canonical node acceptance command failed')
            summary['phase_passed']=True
        except BaseException as e:failure=e;summary['error']=str(e)
        finally:
            signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
            try:self.cleanup();summary['cleanup']=True
            except BaseException as e:summary['cleanup_error']=str(e);failure=e
            if summary['cleanup']:
                try:
                    # Capture successful AND failed actual outputs only after the VM is gone.
                    if self.options.phase=='assemble' and summary.get('phase_passed'):
                        self.journal['build_census']=file_census(self.output/'build',4*1024**3);self.journal['assembled']=True
                    if self.options.phase=='refresh-engine' and summary.get('phase_passed'):
                        actual=file_census(self.output/'build',4*1024**3)
                        def derived(name):
                            p=Path(name)
                            return name.startswith('linux/image/') or (p.parent.as_posix() in ('linux','node-linux') and (p.name in ('stack.layers','image.record','image.digest') or re.fullmatch(r'stack-[1-9][0-9]*\.img',p.name) is not None))
                        before={e['path']:e for e in self.journal['build_census'] if not derived(e['path'])};after={e['path']:e for e in actual if not derived(e['path'])}
                        require(before==after,'refresh changed an unrelated assembled artifact')
                        observed=json.loads((self.output/'evidence'/('turn-'+self.token)/'result.json').read_text())
                        require(observed.get('engine_client_refreshed') is True and observed.get('passed') is True,'actual canonical converter refresh not established')
                        self.journal['build_census']=actual;self.journal['assembled']=True;self.journal['refresh_pending']=False
                    for cpu in (1,2,4):
                        for case in range(1,6):
                            root=self.output/'evidence'/('cpu'+str(cpu))/('case'+str(case))
                            if root.exists():
                                rows=file_census(root,allow_sockets=True)
                                prior=self.journal.get('retained_cases',{}).get(str(cpu),{}).get(str(case))
                                require(prior is None or prior==rows,'previous retained case changed during current turn')
                                self.journal.setdefault('retained_cases',{}).setdefault(str(cpu),{})[str(case)]=rows
                                result=root/'result.json'
                                if result.is_file():
                                    r=json.loads(result.read_text())
                                    if r.get('passed') is True and r.get('cleanup') is True:self.journal.setdefault('cases',{}).setdefault(str(cpu),{})[str(case)]=rows
                    if self.options.phase not in ('assemble','refresh-engine'):require(file_census(self.output/'build',4*1024**3)==self.journal['build_census'],'assembled inputs changed during turn')
                    kd3=self.output/'evidence/kd3'
                    if kd3.exists():
                        newrows=file_census(kd3,allow_sockets=True);newmap={e['path']:e for e in newrows};require(all(newmap.get(e['path'])==e for e in self.journal.get('retained_kd3',[])),'prior KD3 evidence changed during current turn');self.journal['retained_kd3']=newrows;self.journal['kd3_plan_sha256']=self.inputs['kd3']['plan_sha256'] if self.options.phase=='kd3' else self.journal.get('kd3_plan_sha256')
                    io=self.output/'evidence/io'
                    if io.exists():
                        rows=file_census(io,allow_sockets=True);prior=self.journal.get('retained_io');require(prior is None or prior==rows,'retained IO changed');self.journal['retained_io']=rows
                    turn=self.output/'evidence'/('turn-'+self.token)
                    if turn.exists():
                        self.journal.setdefault('turn_evidence',{})[self.token]=file_census(turn)
                        result=turn/'result.json'
                        if result.is_file() and summary.get('phase_passed'):
                            observed=json.loads(result.read_text())
                            require(observed.get('passed') is True,'actual guest phase completion absent')
                            if observed.get('matrix15_passed') is True:self.journal['matrix15_passed']=True
                            if observed.get('kd3_native_complete') is True:self.journal['kd3_native_complete']=True
                            if observed.get('kd3_matrix_passed') is True:self.journal['kd3_matrix_passed']=True
                    self.journal.setdefault('turns',[]).append({'summary':str(self.evidence/'summary.json'),'nonce':self.token,'phase':self.options.phase,'phase_passed':summary.get('phase_passed',False)})
                    atomic_json(self.output/'journal.json',self.journal)
                except BaseException as e:summary['retention_error']=str(e);failure=e
            (self.evidence/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
            if self.busy and summary['cleanup'] and (self.output/'busy/owner').read_text().strip()==self.owner:
                (self.output/'busy/owner').unlink();(self.output/'busy').rmdir()
            if summary['cleanup'] and self.fingerprint==(self.lock.stat().st_dev,self.lock.stat().st_ino) and (self.lock/'owner').read_text().strip()==self.owner:
                (self.lock/'owner').unlink();self.lock.rmdir()
            print(json.dumps({'summary':str(self.evidence/'summary.json'),'phase_passed':summary.get('phase_passed',False),'cleanup':summary['cleanup']}),flush=True)
        return 1 if failure else 0

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--run',action='store_true');p.add_argument('--reuse-assembly');p.add_argument('--supervisor-tls-transition',action='store_true');p.add_argument('--engine-client-transition',action='store_true');p.add_argument('--driver-only-reuse',action='store_true');p.add_argument('--kd3-inputs');p.add_argument('--kd3-count',type=int,default=1);p.add_argument('--inputs',required=True);p.add_argument('--output',required=True);p.add_argument('--phase',choices=('assemble','profile','io','kd3'),required=True);p.add_argument('--cpus',type=int,choices=(1,2,4));p.add_argument('--seconds',type=int,default=1100);o=p.parse_args();o.mode='node-acceptance'
    require(180<=o.seconds<=1100,'total turn bound180..1100');require((o.phase=='profile')==(o.cpus is not None),'only profile requires explicit CPU count')
    inputs=validate_index(o.inputs);out=Path(o.output).absolute();require(out.is_relative_to(ROOT) and out.parent.exists(),'output must be in exact owned proof root');private_directory(out.parent)
    require(not o.supervisor_tls_transition or o.reuse_assembly,'supervisor/TLS correction requires explicit original assembly reuse')
    require(not o.engine_client_transition or (o.reuse_assembly and o.supervisor_tls_transition),'engine client correction requires explicit original assembly/supervisor/TLS reuse')
    require(not o.driver_only_reuse or (o.reuse_assembly and not o.supervisor_tls_transition and not o.engine_client_transition),'driver-only reuse requires separate current assembly fork')
    require(not inputs.get('driver_override') or o.driver_only_reuse or out.exists(),'driver override may only enter via explicit fork')
    require((o.phase=='kd3')==bool(o.kd3_inputs),'KD3 mode requires separate exact prepared bundle plan');require(1<=o.kd3_count<=3,'bounded KD3 turn contains1..3 actual boots')
    if o.kd3_inputs:validate_kd3(o.kd3_inputs,inputs)
    reuse=None
    if o.reuse_assembly:
        require(o.phase=='assemble' and not out.exists() and not out.is_symlink(),'reuse only prepares a new assembled output')
        reuse=reuse_plan(o.reuse_assembly,inputs,o.supervisor_tls_transition,o.engine_client_transition,o.driver_only_reuse)
    journal=None
    if out.exists():
        private_directory(out);jp=private_file(out/'journal.json');journal=json.loads(jp.read_text());z=out.stat();require(journal.get('version')==1,'output journal version differs');require((z.st_dev,z.st_ino,z.st_uid)==(journal['device'],journal['inode'],journal['uid']) and journal['inputs_sha256']==inputs['index_sha256'],'output journal identity/input binding differs')
        require(not (out/'busy').exists(),'prior output use requires explicit cleanup review')
        require(o.phase!='assemble' and journal.get('assembled') is True,'never overwrite/reassemble existing outputs')
        require(file_census(out/'build',4*1024**3)==journal['build_census'],'assembled inputs changed')
        for cpu,cases in journal.get('retained_cases',{}).items():
            for case,rows in cases.items():require(file_census(out/'evidence'/('cpu'+cpu)/('case'+case),allow_sockets=True)==rows,'retained case evidence changed')
        if 'retained_io' in journal:require(file_census(out/'evidence/io',allow_sockets=True)==journal['retained_io'],'retained IO changed')
        for token,rows in journal.get('turn_evidence',{}).items():require(file_census(out/'evidence'/('turn-'+token))==rows,'retained command logs changed')
        if o.phase=='profile':
            require(len(journal.get('cases',{}).get(str(o.cpus),{}))<5,'profile already passed; do not repeat')
            for case in range(1,6):
                casepath=out/'evidence'/('cpu'+str(o.cpus))/('case'+str(case))
                require(not casepath.exists() or str(case) in journal.get('cases',{}).get(str(o.cpus),{}),'failed/partial case retained; explicit reconciliation needed, never overwrite')
        if o.phase=='io':require(not (out/'evidence/io').exists(),'never overwrite prior same-home IO proof')
        if o.phase=='kd3':
            require(journal.get('kd3_plan_sha256',inputs['kd3']['plan_sha256'])==inputs['kd3']['plan_sha256'],'KD3 bundle plan changed after actual execution')
            if 'retained_kd3' in journal:require(file_census(out/'evidence/kd3',allow_sockets=True)==journal['retained_kd3'],'prior KD3 actual evidence changed')
    else:require(o.phase=='assemble','first turn must explicitly assemble')
    if not o.run:print(json.dumps({'prepared_only':True,'no_container_commands':True,'index_sha256':inputs['index_sha256'],'assembly_reuse_prepared':reuse is not None}));return 0
    os.umask(0o077)
    run=NodeAcceptanceRun(o,inputs,journal or {})
    def interrupted(signum,frame):raise RuntimeError('interrupted signal '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:run.acquire()
    except RuntimeError as e:print(str(e),file=sys.stderr);return 75
    try:
        if reuse is not None:
            clone_assembly(o,inputs,reuse,run)
            if o.engine_client_transition:
                run.journal=json.loads((out/'journal.json').read_text());o.phase='refresh-engine'
                return run.execute()
            if run.fingerprint==(run.lock.stat().st_dev,run.lock.stat().st_ino) and (run.lock/'owner').read_text().strip()==run.owner:
                (run.lock/'owner').unlink();run.lock.rmdir()
            return 0
        if journal is None:
            out.mkdir(mode=0o700);(out/'build').mkdir(mode=0o700);(out/'evidence').mkdir(mode=0o700);z=out.stat();journal={'version':1,'device':z.st_dev,'inode':z.st_ino,'uid':z.st_uid,'inputs_sha256':inputs['index_sha256'],'assembled':False,'cases':{}};atomic_json(out/'journal.json',journal)
        run.journal=journal
    except BaseException:
        # No VM or worker exists at this point; release only this exact lease.
        if run.fingerprint==(run.lock.stat().st_dev,run.lock.stat().st_ino) and (run.lock/'owner').read_text().strip()==run.owner:
            (run.lock/'owner').unlink();run.lock.rmdir()
        raise
    return run.execute()
if __name__=='__main__':sys.exit(main())
