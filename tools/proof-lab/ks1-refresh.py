import os,pathlib,subprocess,tempfile,hashlib,json,shutil,stat,tarfile,tomllib,time
os.umask(0o077)
repo=pathlib.Path('/Users/brackly/Desktop/Projects/moruna-kd7a-proof')
root=pathlib.Path('/var/folders/1d/f7v5jtsj3hd43ltj5v94smnr0000gn/T/moruna-v1-codex-tq3aoqyt/linux')
assert shutil.disk_usage(root).free>=8*1024**3
previous=root/'ks1-source-ch5l_3vc'
out=pathlib.Path(tempfile.mkdtemp(prefix='ks1-source-retry-',dir=root)); source=out/'source'; source.mkdir(mode=0o700)
shutil.copytree(previous/'source/vendor',source/'vendor',copy_function=os.link)
shutil.copy2(previous/'cargo-config.generated.toml',out/'cargo-config.generated.toml')
def sha(p):
 h=hashlib.sha256()
 with open(p,'rb') as f:
  for chunk in iter(lambda:f.read(1024*1024),b''):h.update(chunk)
 return h.hexdigest()
def save(name,value):
 p=out/name;p.write_text(json.dumps(value,sort_keys=True,indent=2)+'\n');return sha(p)
def selected():
 paths=subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard','-z'],cwd=repo).split(b'\0');result=[]
 for raw in paths:
  if not raw:continue
  rel=pathlib.Path(os.fsdecode(raw))
  if any(x in ['.git','.vmrun','.venv','node_modules'] or x.startswith('target') for x in rel.parts):continue
  p=repo/rel
  if not p.exists():continue
  assert not p.is_symlink(),str(rel)
  if p.is_file():result.append({'path':str(rel),'type':'file','bytes':p.stat().st_size,'mode':stat.S_IMODE(p.stat().st_mode),'sha256':sha(p)})
 return sorted(result,key=lambda x:x['path'])
before=selected(); head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip();diff=subprocess.check_output(['git','diff','--binary','HEAD'],cwd=repo)
locksha=sha(repo/'Cargo.lock')
def external_pins(lock):
 return sorted((x['name'],x['version'],x['source'],x.get('checksum')) for x in tomllib.load(open(lock,'rb'))['package'] if x.get('source'))
assert external_pins(repo/'Cargo.lock')==external_pins(previous/'source/Cargo.lock'),'external package graph changed: resolve verified vendor anew'
save('source-manifest.json',before)
for item in before:
 dest=source/item['path'];dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(repo/item['path'],dest)
cmd=['cargo','+1.98.1','vendor','--offline','--locked','--versioned-dirs','--manifest-path','Cargo.toml','vendor']
assert (source/'vendor').is_dir() and (out/'cargo-config.generated.toml').is_file()
assert sha(source/'Cargo.lock')==locksha
config=(out/'cargo-config.generated.toml').read_text()
config=config.replace(str(previous/'source/vendor'),'vendor').replace(str(source/'vendor'),'vendor');assert '/Users/' not in config and str(root) not in config
(source/'.cargo').mkdir(exist_ok=True)
config_path=source/'.cargo/config.toml'
original_config=config_path.read_text() if config_path.is_file() else ''
original=tomllib.loads(original_config)
assert 'source' not in original,'existing source tables need explicit reconciliation, never overwrite policy'
config=original_config+'\n'+config
tomllib.loads(config)
config_path.write_text(config)
locked=tomllib.load(open(source/'Cargo.lock','rb'))['package'];registry=[p for p in locked if p.get('source','').startswith('registry+')];git=[p for p in locked if p.get('source','').startswith('git+')]
checks=[];size=0
for crate in sorted((source/'vendor').iterdir()):
 assert crate.is_dir() and not crate.is_symlink()
 manifest=json.load(open(crate/'.cargo-checksum.json'))
 cargo=tomllib.load(open(crate/'Cargo.toml','rb'));package=cargo['package'];matching=[p for p in locked if p['name']==package['name'] and p['version']==package['version'] and p.get('source')]
 assert len(matching)==1,(crate,matching)
 if matching[0].get('checksum'):assert manifest['package']==matching[0]['checksum']
 for rel,digest in manifest['files'].items():assert sha(crate/rel)==digest,(crate,rel)
 checks.append({'name':package['name'],'version':package['version'],'source':matching[0]['source'],'checksum_manifest_sha256':sha(crate/'.cargo-checksum.json'),'files_checked':len(manifest['files'])})
 for p in crate.rglob('*'):
  assert not p.is_symlink(),str(p)
  if p.is_file():size+=p.stat().st_size
assert len(checks)==len(registry)+len(git),(len(checks),len(registry),len(git))
assert size<=1024**3,size
vendor_sha=save('vendor-manifest.json',checks)
inputs=[]
for p in sorted(source.rglob('*')):
 assert not p.is_symlink(),str(p)
 if p.is_file():inputs.append({'path':str(p.relative_to(source)),'type':'file','bytes':p.stat().st_size,'mode':stat.S_IMODE(p.stat().st_mode),'sha256':sha(p)})
input_sha=save('inputs-manifest.json',inputs)
archive=out/'ks1-inputs.tar.gz'
with tarfile.open(archive,'w:gz',compresslevel=1) as tar:
 for item in inputs:
  p=source/item['path'];info=tar.gettarinfo(str(p),arcname=item['path']);info.uid=info.gid=0;info.uname=info.gname='';info.mtime=0
  with open(p,'rb') as f:tar.addfile(info,f)
expected={p['path']:p for p in inputs};found=set()
with tarfile.open(archive) as tar:
 for member in tar:
  assert member.isfile() and member.name in expected and member.name not in found
  item=expected[member.name];assert member.size==item['bytes'] and member.mode==item['mode']
  h=hashlib.sha256();f=tar.extractfile(member)
  for chunk in iter(lambda:f.read(1024*1024),b''):h.update(chunk)
  assert h.hexdigest()==item['sha256'];found.add(member.name)
assert found==set(expected)
assert selected()==before,'source changed during preparation'
assert subprocess.check_output(['git','diff','--binary','HEAD'],cwd=repo)==diff
provenance={'source_head':head,'source_dirty_diff_sha256':hashlib.sha256(diff).hexdigest(),'source_census_entries':len(before),'source_census_file':'source-manifest.json','source_census_sha256':sha(out/'source-manifest.json'),'cargo_lock_sha256':locksha,'copied_lock_unchanged':sha(source/'Cargo.lock')==locksha,'registry_packages':len(registry),'git_packages':len(git),'vendor_bytes':size,'vendor_files_checked':sum(c['files_checked'] for c in checks),'vendor_manifest_sha256':vendor_sha,'vendor_command':cmd,'portable_vendor_config_sha256':sha(source/'.cargo/config.toml'),'inputs_census_path':str(out/'inputs-manifest.json'),'inputs_census_sha256':input_sha,'inputs_files':len(inputs),'inputs_bytes':sum(i['bytes'] for i in inputs),'archive_path':str(archive),'archive_sha256':sha(archive),'archive_bytes':archive.stat().st_size,'archive_roundtrip_verified':True,'source_unchanged_after_archive':True,'scope':'Full original workspace source and unchanged lock; all exact locked registry/git sources vendored offline. No compiler/tests/VM/network; no .git/target/.vmrun/.venv/home credentials.'}
save('provenance.json',provenance);print(json.dumps({'path':str(out),**provenance}),flush=True)
