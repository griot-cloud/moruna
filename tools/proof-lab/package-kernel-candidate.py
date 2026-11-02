"""Freeze assembled kernel sources with the independently verified 147-package graph."""
import hashlib, json, os, shutil, stat, sys, tarfile, tempfile, tomllib
from pathlib import Path

os.umask(0o077)
root=Path(__file__).resolve().parent
candidate=Path(sys.argv[1]).resolve(strict=True)
assert candidate.parent==root and candidate.name.startswith('std-integration-candidate-')
source_input=candidate/'source'; vendor_input=root/'kernel-vendor-ztk_vj46'
prior=json.loads((vendor_input/'provenance.json').read_text())

def sha(path):
    with path.open('rb') as file: return hashlib.file_digest(file, 'sha256').hexdigest()

def census(folder, authored=False):
    values=[]
    for path in sorted(folder.rglob('*')):
        rel=path.relative_to(folder)
        if authored and (rel.parts[0] in {'.git','.vmrun','.venv','node_modules','target','target-linux','vendor'} or rel.parts[0].startswith('target-')): continue
        info=path.lstat(); assert not path.is_symlink()
        if stat.S_ISREG(info.st_mode):
            values.append({'path':str(rel),'type':'file','bytes':info.st_size,'mode':stat.S_IMODE(info.st_mode),'sha256':sha(path)})
    return values

current_lock=sha(source_input/'kernel/Cargo.lock')
release_delta=None
if len(sys.argv)>3:
    anchor_path=Path(sys.argv[3]).resolve(strict=True)
    anchor=json.loads(anchor_path.read_text())
    old_lock=anchor_path.parent/'source/kernel/Cargo.lock'
    assert sha(old_lock)==anchor['kernel_cargo_lock_sha256']
    assert sha(Path(anchor['archive_path']))==anchor['archive_sha256']
    old_packages=tomllib.loads(old_lock.read_text())['package']
    new_packages=tomllib.loads((source_input/'kernel/Cargo.lock').read_text())['package']
    assert [p for p in old_packages if p.get('source')]==[p for p in new_packages if p.get('source')], 'release external pin drift'
    old_local={p['name']:p for p in old_packages if not p.get('source')}
    new_local={p['name']:p for p in new_packages if not p.get('source')}
    assert old_local.keys()==new_local.keys()
    changed={n for n in old_local if old_local[n]!=new_local[n]}
    assert changed=={'moruna-record','moruna-storage-format'}, 'unexpected kernel release delta'
    for name in changed:
        before=old_local[name];after=new_local[name]
        assert before['version']=='0.5.9' and after['version']=='0.6.0'
        assert {k:v for k,v in before.items() if k!='version'}=={k:v for k,v in after.items() if k!='version'}
    release_delta={'anchor_provenance':str(anchor_path),'anchor_archive_sha256':anchor['archive_sha256'],
                   'old_lock_sha256':sha(old_lock),'new_lock_sha256':current_lock,'local_versions':sorted(changed)}
    # Replay the existing dependency reconciliation against its exact source
    # anchor; the output below still records the actual new lock and sources.
    current_lock=sha(old_lock)
removed_registry=set()
if current_lock != prior['kernel_lock_sha256']:
    reconciliation=json.loads((root/'kernel-lock-reconciliation-20261030/reconciliation.json').read_text())
    assert reconciliation['before_sha256']==prior['kernel_lock_sha256']
    assert not reconciliation['external_added'] and not reconciliation['external_removed'], 'unreviewed external graph change'
    expected_lock=reconciliation['after_sha256']
    if current_lock != expected_lock:
        ring_change=json.loads((root/'kernel-ring-lock-reconciliation-20261030/reconciliation.json').read_text())
        assert ring_change['before_sha256']==expected_lock
        if ring_change['after_sha256']!=current_lock:
            io_root=root/'kernel-native-io-lock-reconciliation-20261030'
            io_change=json.loads((io_root/'reconciliation.json').read_text())
            assert io_change['before_sha256']==ring_change['after_sha256'] and io_change['after_sha256']==current_lock
            assert sha(io_root/'Cargo.lock.before')==io_change['before_sha256'] and sha(io_root/'Cargo.lock.after')==current_lock
            def external_pins(path):
                return sorted((p['name'],p['version'],p.get('source'),p.get('checksum')) for p in tomllib.loads(path.read_text())['package'] if p.get('source'))
            assert io_change['metadata_exit']==0 and io_change['external_pins_unchanged'] and external_pins(io_root/'Cargo.lock.before')==external_pins(io_root/'Cargo.lock.after')
            assert io_change['external_count']==146
        assert not ring_change['external_added'] and ring_change['external_removed']==[['ring','0.17.14','registry+https://github.com/rust-lang/crates.io-index','a4689e6c2294d81e88dc6261c768b63bc4fcdb852be6d1352498b114f61383b7']]
        removed_registry.add(('ring','0.17.14'))
out=Path(tempfile.mkdtemp(prefix='kernel-inputs-',dir=root)); source=out/'source'; source.mkdir(mode=0o700)
before=census(source_input,authored=True)
assert not any(b'<<<<<<< integration\n' in (source_input/item['path']).read_bytes() for item in before), 'unresolved integration merge markers'
for item in before:
    path=source/item['path'];path.parent.mkdir(parents=True,exist_ok=True)
    shutil.copy2(source_input/item['path'],path)
(source/'vendor').mkdir(mode=0o700)
removed_files=0
vendor_root=vendor_input/'vendor'
if release_delta:
    # The verified immutable archive remains complete even when an old mutable
    # vendor cache has lost files. Recover only into this new proof input.
    manifest=Path(anchor['inputs_census_path'])
    assert sha(manifest)==anchor['inputs_census_sha256']
    records={p['path']:p for p in json.loads(manifest.read_text()) if p['path'].startswith('vendor/')}
    recovered=out/'verified-parent-vendor';recovered.mkdir(mode=0o700)
    seen=set()
    with tarfile.open(anchor['archive_path']) as tar:
        for member in tar:
            if member.name not in records:continue
            item=records[member.name]
            assert member.isfile() and not Path(member.name).is_absolute() and '..' not in Path(member.name).parts
            assert member.name not in seen and member.size==item['bytes']
            seen.add(member.name);dest=recovered/member.name;dest.parent.mkdir(parents=True,exist_ok=True)
            with tar.extractfile(member) as src,dest.open('xb') as target:shutil.copyfileobj(src,target)
            dest.chmod(item['mode']);assert sha(dest)==item['sha256']
    assert seen==records.keys(), 'immutable parent vendor census incomplete'
    vendor_root=recovered/'vendor'
for directory in vendor_root.iterdir():
    package=tomllib.loads((directory/'Cargo.toml').read_text())['package']
    if (package['name'],package['version']) in removed_registry:
        removed_files+=len(json.loads((directory/'.cargo-checksum.json').read_text())['files'])
        continue
    shutil.copytree(directory,source/'vendor'/directory.name,copy_function=os.link)
stdlib=None
if len(sys.argv)>2:
    extra=Path(sys.argv[2]).resolve(strict=True)
    assert extra.parent==root and extra.name.startswith('stdlib-vendor-')
    stdlib=json.loads((extra/'provenance.json').read_text());assert stdlib['complete']
    assert sha(Path(stdlib['actual_rust_src_lock']))==stdlib['actual_rust_src_lock_sha256']
    for directory in (extra/'vendor').iterdir():
        if release_delta and (source/'vendor'/directory.name).exists():continue
        assert not (source/'vendor'/directory.name).exists()
        shutil.copytree(directory,source/'vendor'/directory.name,copy_function=os.link)
locked=tomllib.loads((source/'kernel/Cargo.lock').read_text())['package']
external={(p['name'],p['version']):p for p in locked if p.get('source')}
kernel_count=len(external)
if stdlib:
    for package in stdlib['packages']:
        key=(package['name'],package['version']);assert key not in external
        external[key]={'checksum':package['checksum']}
packages=[]; checked=0
for crate in sorted((source/'vendor').iterdir()):
    checksum=json.loads((crate/'.cargo-checksum.json').read_text())
    package=tomllib.loads((crate/'Cargo.toml').read_text())['package']; key=(package['name'],package['version'])
    assert key in external and checksum['package']==external[key]['checksum']
    for name,digest in checksum['files'].items(): assert sha(crate/name)==digest
    checked+=len(checksum['files']);packages.append(key)
assert len(packages)==len(external) and kernel_count==prior['registry_packages']-len(removed_registry)
assert checked==(anchor['verified_vendor_files'] if release_delta else prior['verified_vendor_files']-removed_files+(sum(p['files_checked'] for p in stdlib['packages']) if stdlib else 0))
if removed_registry:
    provenance=json.loads((source/'kernel/net/ring-provenance.json').read_text())
    upstream=source/'kernel/net'/provenance['upstream_files_manifest']
    assert sha(upstream)==provenance['upstream_files_manifest_sha256']
    expected={line.split('  ',1)[1]:line.split('  ',1)[0] for line in upstream.read_text().splitlines()}
    directory=source/'kernel/vendor/ring-0.17.14'
    actual={str(p.relative_to(directory)) for p in directory.rglob('*') if p.is_file()}
    assert actual==expected.keys() and len(actual)==391
    assert [name for name,digest in expected.items() if sha(directory/name)!=digest]==['build.rs']
    assert sha(directory/'build.rs')==provenance['patched_build_rs_sha256']
config=source/'.cargo/config.toml'; original=config.read_text()
assert 'source' not in tomllib.loads(original)
generated=(vendor_input/'config.generated.toml').read_text()
import re
generated=re.sub(r'directory = "[^"]+"','directory = "vendor"',generated)
config.write_text(original+'\n'+generated);tomllib.loads(config.read_text())

def save(name,value):
    p=out/name;p.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n');return sha(p)

source_sha=save('source-manifest.json',before)
inputs=census(source);input_sha=save('inputs-manifest.json',inputs)
archive=out/'kernel-inputs.tar.gz'
with tarfile.open(archive,'w:gz',compresslevel=1) as tar:
    for item in inputs:
        path=source/item['path'];member=tar.gettarinfo(str(path),arcname=item['path'])
        member.uid=member.gid=0;member.uname=member.gname='';member.mtime=0
        with path.open('rb') as file:tar.addfile(member,file)
assert census(source_input,authored=True)==before, 'source changed during freeze'
capture=json.loads((candidate/'capture.json').read_text()) if (candidate/'capture.json').is_file() else {}
record={'source_head':capture.get('source_head','38447a99447151aec77eb28272a8a32e682052f3'),
        'source_dirty_diff_sha256':source_sha,'source_census_sha256':source_sha,
        'kernel_cargo_lock_sha256':sha(source/'kernel/Cargo.lock'),
        'source_census_entries':len(before),'registry_packages':len(packages),'verified_vendor_files':checked,
        'kernel_graph_packages':kernel_count,'stdlib_provenance':stdlib,
        'path_provider_adaptation':sorted(removed_registry),
        'inputs_census_path':str(out/'inputs-manifest.json'),'inputs_census_sha256':input_sha,
        'inputs_files':len(inputs),'inputs_bytes':sum(i['bytes'] for i in inputs),
        'archive_path':str(archive),'archive_sha256':sha(archive),'archive_bytes':archive.stat().st_size,
        'candidate_path':str(candidate),'source_unchanged_after_archive':True,
        'scope':'Assembled kernel graph host/native source input. No compiler, boot, deployment or hardware proof implied.'}
if release_delta:record['release_local_version_delta']=release_delta
if capture:record['repository_capture']=capture
save('provenance.json',record)
print(json.dumps({'path':str(out),'inputs_files':len(inputs),'inputs_bytes':record['inputs_bytes'],
                  'archive_sha256':record['archive_sha256'],'kernel_lock_sha256':record['kernel_cargo_lock_sha256']}))
