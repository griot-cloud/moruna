"""Prepare exact Rust stdlib registry source inputs; no host installation or execution."""
import concurrent.futures,hashlib,io,json,os,pathlib,shutil,subprocess,tarfile,tempfile,urllib.parse
from run import ROOT,require,sha

os.umask(0o077)
availability=json.loads((ROOT/'stdlib-source-availability.json').read_text())
out=pathlib.Path(tempfile.mkdtemp(prefix='stdlib-vendor-',dir=ROOT));vendor=out/'vendor';vendor.mkdir(mode=0o700)
lock=ROOT/'moruna-proof-501-110123c01342/native-inventory/stdlib-0.lock'
require(lock.is_file(),'actual cached rust-src lock missing')
lock_sha=sha(lock);records=[]

def validate(directory,expected):
    manifest=json.loads((directory/'.cargo-checksum.json').read_text())
    require(manifest['package']==expected['checksum'],'stdlib package checksum differs')
    for name,digest in manifest['files'].items():
        p=directory/name;require(not p.is_symlink() and p.resolve(strict=True).is_relative_to(directory) and sha(p)==digest,'stdlib source checksum differs')
    require(not any(p.is_symlink() for p in directory.rglob('*')),'stdlib source links not accepted')
    return len(manifest['files'])

for package in availability['cached']:
    source=pathlib.Path(package['cached'][0]);source=source if source.is_absolute() else ROOT/source
    checked=validate(source,package);dest=vendor/(package['name']+'-'+package['version'])
    shutil.copytree(source,dest,copy_function=os.link);validate(dest,package)
    records.append({**{k:package[k] for k in ['name','version','checksum']},'source':str(source),'files_checked':checked})

def fetch(package):
    name=package['name'];version=package['version'];prefix=name+'-'+version
    url='https://static.crates.io/crates/'+urllib.parse.quote(name,safe='')+'/'+urllib.parse.quote(prefix,safe='')+'.crate'
    archive=out/(prefix+'.crate')
    # Use the OS trust store via system curl; never bypass TLS verification.
    response=subprocess.run(['/usr/bin/curl','--fail','--silent','--show-error','--location',
        '--proto','=https','--proto-redir','=https','--max-time','30','--max-filesize',str(16*1024*1024),
        '--output',str(archive),'--write-out','%{url_effective}',url],text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    require(response.returncode==0,'verified source download failed: '+response.stderr.strip())
    require(response.stdout.startswith('https://static.crates.io/'),'crate redirect outside official source')
    data=archive.read_bytes()
    require(len(data)<=16*1024*1024 and hashlib.sha256(data).hexdigest()==package['checksum'],'downloaded stdlib crate checksum/size differs')
    dest=vendor/prefix;dest.mkdir(mode=0o700)
    checksums={};total=0
    with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as source:
        for member in source:
            p=pathlib.PurePosixPath(member.name)
            require(not p.is_absolute() and '..' not in p.parts and p.parts[0]==prefix,'crate archive path escape')
            if member.isdir():continue
            require(member.isfile(),'crate link/device refused')
            rel=pathlib.Path(*p.parts[1:]);require(rel.parts and str(rel) not in checksums,'duplicate crate file')
            content=source.extractfile(member).read();total+=len(content);require(total<=64*1024*1024,'crate expanded size bound')
            target=dest/rel;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(content)
            target.chmod(0o555 if member.mode&0o111 else 0o444);checksums[str(rel)]=hashlib.sha256(content).hexdigest()
    (dest/'.cargo-checksum.json').write_text(json.dumps({'files':checksums,'package':package['checksum']},sort_keys=True)+'\n')
    checked=validate(dest,package)
    return {**{k:package[k] for k in ['name','version','checksum']},'url':url,'archive_bytes':len(data),'files_checked':checked}

with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
    futures={pool.submit(fetch,p):p for p in availability['missing']}
    for future in concurrent.futures.as_completed(futures):
        p=futures[future]
        try:
            record=future.result();records.append(record);print(json.dumps({'verified':record['name'],'version':record['version'],'bytes':record['archive_bytes']}),flush=True)
        except Exception as error:
            records.append({'name':p['name'],'version':p['version'],'error':type(error).__name__+': '+str(error)})
            print(json.dumps(records[-1]),flush=True)
records.sort(key=lambda p:(p['name'],p['version']))
value={'actual_rust_src_lock':str(lock),'actual_rust_src_lock_sha256':lock_sha,'purpose':'Offline native build-std source inputs, no compiled capability claim','packages':records,'complete':not any('error' in p for p in records)}
(out/'provenance.json').write_text(json.dumps(value,indent=2)+'\n');print(json.dumps({'path':str(out),'complete':value['complete'],'packages':len(records)}))
