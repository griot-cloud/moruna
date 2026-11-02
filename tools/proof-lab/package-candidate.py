"""Freeze an owned combined source candidate with unchanged verified vendor inputs."""
import hashlib,json,os,shutil,stat,sys,tarfile,tempfile,tomllib
from pathlib import Path

os.umask(0o077)
root=Path(__file__).resolve().parent
candidate=Path(sys.argv[1]).resolve(strict=True)
assert candidate.parent==root and candidate.name.startswith('std-integration-candidate-')
assert shutil.disk_usage(root).free>7*1024**3,'bounded compile preparation disk floor'
previous=root/'ks1-source-ch5l_3vc'
source_input=candidate/'source'
out=Path(tempfile.mkdtemp(prefix='combined-inputs-',dir=root));source=out/'source';source.mkdir(mode=0o700)

def sha(path):
 d=hashlib.sha256()
 with path.open('rb') as f:
  for chunk in iter(lambda:f.read(1024*1024),b''):d.update(chunk)
 return d.hexdigest()

def census(folder,filter_source=False):
 entries=[]
 for path in sorted(folder.rglob('*')):
  relative=path.relative_to(folder)
  # Build-output filtering belongs to authored sources only. Locked vendor
  # packages such as target-lexicon are real dependencies and must remain whole.
  if filter_source and (relative.parts[0] in {'.git','.vmrun','.venv','node_modules','target','target-linux'} or relative.parts[0].startswith('target-')):continue
  info=path.lstat();assert not path.is_symlink()
  if path.is_file():entries.append({'path':str(path.relative_to(folder)),'type':'file','bytes':info.st_size,'mode':stat.S_IMODE(info.st_mode),'sha256':sha(path)})
 return entries

def save(name,value):
 p=out/name;p.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n');return sha(p)

def external(path):
 return sorted((x['name'],x['version'],x['source'],x.get('checksum')) for x in tomllib.loads(path.read_text())['package'] if x.get('source'))

assert external(source_input/'Cargo.lock')==external(previous/'source/Cargo.lock'),'external pin drift'
before=census(source_input,filter_source=True)
assert not any(b'<<<<<<< integration\n' in (source_input/item['path']).read_bytes() for item in before), 'unresolved integration merge markers'
for item in before:
 dest=source/item['path'];dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(source_input/item['path'],dest)
source_sha=save('source-manifest.json',before)
vendor_source=previous/'source/vendor'
vendor_fallback=None
if len(sys.argv)>2:
 accepted_path=Path(sys.argv[2]).resolve(strict=True);accepted=json.loads(accepted_path.read_text())
 accepted_archive=Path(accepted['archive_path']).resolve(strict=True)
 assert sha(accepted_archive)==accepted['archive_sha256'],'accepted vendor archive SHA differs'
 accepted_manifest=Path(accepted['inputs_census_path']).resolve(strict=True)
 assert sha(accepted_manifest)==accepted['inputs_census_sha256'],'accepted input census differs'
 accepted_records={i['path']:i for i in json.loads(accepted_manifest.read_text())}
 vendor_records={n:i for n,i in accepted_records.items() if n.startswith('vendor/')}
 recovery=out/'verified-vendor-input';recovery.mkdir(mode=0o700);seen=set()
 with tarfile.open(accepted_archive) as tar:
  for entry in tar:
   if entry.name not in vendor_records:continue
   item=vendor_records[entry.name];assert entry.isfile() and not Path(entry.name).is_absolute() and '..' not in Path(entry.name).parts
   assert entry.name not in seen and entry.size==item['bytes'];seen.add(entry.name)
   dest=recovery/entry.name;dest.parent.mkdir(parents=True,exist_ok=True)
   with tar.extractfile(entry) as src,dest.open('xb') as target:shutil.copyfileobj(src,target)
   dest.chmod(item['mode']);assert sha(dest)==item['sha256'],'recovered vendor bytes differ'
 assert seen==set(vendor_records),'accepted archive vendor incomplete'
 # Local workspace release versions may change while the dependency closure stays
 # identical. The complete external source/version/checksum pins must still match.
 assert external(accepted_path.parent/'source/Cargo.lock')==external(source/'Cargo.lock'),'accepted external pins changed'
 vendor_source=recovery/'vendor'
 vendor_fallback={'provenance':str(accepted_path),'provenance_sha256':sha(accepted_path),'archive_sha256':accepted['archive_sha256'],'inputs_census_sha256':accepted['inputs_census_sha256'],'vendor_files_verified':len(seen),'reason':'canonical old vendor source is incomplete; verified immutable accepted archive used without package or config changes'}
shutil.copytree(vendor_source,source/'vendor',copy_function=os.link)
config=(previous/'cargo-config.generated.toml').read_text().replace(str(previous/'source/vendor'),'vendor')
assert '/Users/' not in config and str(root) not in config
config_path=source/'.cargo/config.toml';original=config_path.read_text() if config_path.is_file() else ''
assert 'source' not in tomllib.loads(original),'explicit source-policy reconciliation needed'
config_path.parent.mkdir(exist_ok=True);config_path.write_text(original+'\n'+config);tomllib.loads(config_path.read_text())
if vendor_fallback:assert sha(config_path)==accepted_records['.cargo/config.toml']['sha256'],'portable accepted compiler/source config changed'
vendor=[];locked=tomllib.loads((source/'Cargo.lock').read_text())['package']
for crate in sorted((source/'vendor').iterdir()):
 assert crate.is_dir() and not crate.is_symlink()
 checksum=json.loads((crate/'.cargo-checksum.json').read_text());package=tomllib.loads((crate/'Cargo.toml').read_text())['package']
 matching=[p for p in locked if p['name']==package['name'] and p['version']==package['version'] and p.get('source')]
 assert len(matching)==1
 if matching[0].get('checksum'):assert checksum['package']==matching[0]['checksum']
 for name,digest in checksum['files'].items():assert sha(crate/name)==digest
 vendor.append({'name':package['name'],'version':package['version'],'source':matching[0]['source'],'files_checked':len(checksum['files']),'checksum_manifest_sha256':sha(crate/'.cargo-checksum.json')})
assert len(vendor)==len(external(source/'Cargo.lock'))
vendor_sha=save('vendor-manifest.json',vendor)
inputs=census(source);input_sha=save('inputs-manifest.json',inputs)
archive=out/'combined-inputs.tar.gz'
with tarfile.open(archive,'w:gz',compresslevel=1) as tar:
 for item in inputs:
  path=source/item['path'];entry=tar.gettarinfo(str(path),arcname=item['path']);entry.uid=entry.gid=0;entry.uname=entry.gname='';entry.mtime=0
  with path.open('rb') as f:tar.addfile(entry,f)
assert census(source_input,filter_source=True)==before,'candidate changed during freeze'
capture=json.loads((candidate/'capture.json').read_text()) if (candidate/'capture.json').is_file() else {}
provenance={'source_head':capture.get('source_head','38447a99447151aec77eb28272a8a32e682052f3'),'source_dirty_diff_sha256':source_sha,
 'source_census_entries':len(before),'source_census_file':'source-manifest.json','source_census_sha256':source_sha,
 'cargo_lock_sha256':sha(source/'Cargo.lock'),'registry_packages':sum(v['source'].startswith('registry+') for v in vendor),
 'git_packages':sum(v['source'].startswith('git+') for v in vendor),'vendor_manifest_sha256':vendor_sha,
 'portable_vendor_config_sha256':sha(config_path),'inputs_census_path':str(out/'inputs-manifest.json'),
 'inputs_census_sha256':input_sha,'inputs_files':len(inputs),'inputs_bytes':sum(i['bytes'] for i in inputs),
 'archive_path':str(archive),'archive_sha256':sha(archive),'archive_bytes':archive.stat().st_size,
 'source_unchanged_after_archive':True,'candidate_path':str(candidate),
 'scope':'Frozen source input only; compilation, execution and release acceptance are separate recorded gates.'}
if capture:provenance['repository_capture']=capture
if vendor_fallback:provenance['verified_vendor_fallback']=vendor_fallback
save('provenance.json',provenance)
print(json.dumps({'path':str(out),'source_census_sha256':source_sha,'inputs_census_sha256':input_sha,'archive_sha256':provenance['archive_sha256'],'source_files':len(before),'inputs_files':len(inputs)}))
