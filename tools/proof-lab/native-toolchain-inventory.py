"""Guest-only evidence of actual cached native toolchain and checksum-verified std sources."""
import hashlib,json,pathlib,shutil,subprocess,tomllib

out=pathlib.Path('/proof/native-inventory');out.mkdir(mode=0o700)
record={'scope':'Actual cache inventory only, not a native compile or boot result','locks':[],'exported':[],'missing':[]}
def sha(path):
    with path.open('rb') as file:return hashlib.file_digest(file,'sha256').hexdigest()
try:
    root=pathlib.Path(subprocess.check_output(['rustup','run','nightly-2026-07-21','rustc','--print','sysroot'],text=True).strip())
    library=root/'lib/rustlib/src/rust/library'
    record['sysroot']=str(root);record['core_source_present']=(library/'core/Cargo.toml').is_file()
    exported=set();total=0
    for lock in [library/'Cargo.lock',library.parent/'Cargo.lock']:
        if not lock.is_file():continue
        name='stdlib-'+str(len(record['locks']))+'.lock';shutil.copyfile(lock,out/name)
        record['locks'].append({'path':str(lock),'evidence':name,'sha256':sha(lock)})
        for package in tomllib.loads(lock.read_text())['package']:
            if not package.get('source','').startswith('registry+') or (package['name'],package['version']) in exported:continue
            exported.add((package['name'],package['version']))
            directory=package['name']+'-'+package['version']
            if (pathlib.Path('/proof/source/vendor')/directory).is_dir():continue
            matches=list(pathlib.Path('/usr/local/cargo/registry/src').glob('*/'+directory))
            if len(matches)!=1:
                record['missing'].append({'name':package['name'],'version':package['version'],'checksum':package.get('checksum')});continue
            source=matches[0];checksum=source/'.cargo-checksum.json'
            if not checksum.is_file():raise RuntimeError('cached source lacks checksum: '+directory)
            manifest=json.loads(checksum.read_text())
            if manifest['package']!=package['checksum']:raise RuntimeError('cached crate checksum differs: '+directory)
            for relative,digest in manifest['files'].items():
                path=source/relative
                if path.is_symlink() or not path.resolve(strict=True).is_relative_to(source) or sha(path)!=digest:raise RuntimeError('cached source bytes differ: '+directory+'/'+relative)
            for path in source.rglob('*'):
                if path.is_symlink():raise RuntimeError('cached source link not exported: '+str(path))
                if path.is_file():total+=path.stat().st_size
            if total>64*1024*1024:raise RuntimeError('stdlib extra export exceeds64MiB')
            destination=out/'vendor-extra'/directory;destination.parent.mkdir(mode=0o700,exist_ok=True)
            shutil.copytree(source,destination)
            for relative,digest in manifest['files'].items():
                if sha(destination/relative)!=digest:raise RuntimeError('exported crate checksum differs')
            record['exported'].append({'name':package['name'],'version':package['version'],'checksum':package['checksum'],'files_checked':len(manifest['files'])})
    record['exported_bytes']=total
except Exception as error:
    record['error']=type(error).__name__+': '+str(error)
(out/'inventory.json').write_text(json.dumps(record,indent=2)+'\n')
print('MORUNA_NATIVE_TOOLCHAIN_INVENTORY',json.dumps({k:record.get(k) for k in ['core_source_present','exported_bytes','missing','error']}))
