"""Extract exact authenticated cross compiler inputs, never install or execute them."""
import gzip,hashlib,io,json,os,posixpath,tarfile,tempfile
from pathlib import Path,PurePosixPath
from run import ROOT,require,sha
os.umask(0o077)
lock=ROOT/'moruna-env-501-7bd45bcd7fdb/package-resolution/package-lock.json'
require(sha(lock)=='fbd4eeb24b327cd13e500ef5f44fc57a2fb4f8d3e3688185dabeecd300a64bbc','reviewed lock changed')
source=json.loads(lock.read_text());selected={p['package'] for p in source['packages'] if p['package'].startswith(('gcc-','cpp-','binutils-x86','libgcc-','libc6-amd64','libstdc++6-amd64','libasan8-amd64','libatomic1-amd64','libgomp1-amd64','libitm1-amd64','liblsan0-amd64','libquadmath0-amd64','libtsan2-amd64','libubsan1-amd64'))}|{'musl','musl-dev'}
packages=[]
for p in source['packages']:
 if p['package'] in selected:packages.append({**p,'file':str(ROOT/'package-cache/47557ae5b862b33ee072e8a18d240437/debs'/Path(p['path']).name)})
for p in json.loads((ROOT/'missing-gnu-cross-headers.json').read_text())['packages']:
 packages.append({'package':p['Package'],'version':p['Version'],'architecture':p['Architecture'],'sha256':p['SHA256'],'bytes':int(p['Size']),'file':str(ROOT/'cross-headers-debs'/Path(p['Filename']).name)})
supplement=ROOT/'supplemental-libcc1-lock.json'
supplement_sha=sha(supplement)
for p in json.loads(supplement.read_text())['packages']:
 require(p['Package']=='libcc1-0' and p['Architecture']=='arm64' and p['Version']=='12.2.0-14+deb12u1','unexpected supplemental input')
 packages.append({'package':p['Package'],'version':p['Version'],'architecture':p['Architecture'],'sha256':p['SHA256'],'bytes':int(p['Size']),'file':str(ROOT/'supplemental-libcc1-debs'/Path(p['Filename']).name)})
def payload(path):
 with path.open('rb') as f:
  require(f.read(8)==b'!<arch>\n','bad deb')
  while header:=f.read(60):
   require(len(header)==60 and header[58:]==b'`\n','bad ar');name=header[:16].decode().strip().rstrip('/');size=int(header[48:58]);data=f.read(size);require(len(data)==size,'short ar')
   if size%2:require(f.read(1)==b'\n','ar alignment')
   if name=='data.tar.xz':return data
 raise RuntimeError('data missing')
# Linux names never become host filesystem paths. The virtual manifest is case-sensitive.
MAX_BYTES=512*1024**2
MAX_MEMBERS=100000
out=Path(tempfile.mkdtemp(prefix='cross-tools-archive-',dir=ROOT))
archive_out=out/'cross-tools.tar.gz'
items={};total=0;seen=0;expanded=0
headers=ROOT/'missing-gnu-cross-headers.json'
headers_sha=sha(headers)
def clean(name):
 require(isinstance(name,str) and len(name.encode())<=4096 and '\x00' not in name and '\\' not in name,'unsafe name')
 while name.startswith('./'):name=name[2:]
 p=PurePosixPath(name)
 require(not p.is_absolute() and '..' not in p.parts,'unsafe archive path '+name)
 return str(p) if str(p)!='.' else ''
def emit_dir(name,tar):
 if not name:return
 prior=items.get(name)
 if prior:
  require(prior['type']=='directory','non-directory parent '+name);return
 emit_dir(posixpath.dirname(name),tar)
 m=tarfile.TarInfo(name);m.type=tarfile.DIRTYPE;m.mode=0o555
 tar.addfile(m);items[name]={'path':name,'type':'directory'}
with archive_out.open('xb') as raw:
 with gzip.GzipFile(filename='',mode='wb',fileobj=raw,mtime=0,compresslevel=6) as gz:
  with tarfile.open(fileobj=gz,mode='w|',format=tarfile.PAX_FORMAT) as dest:
   for package in packages:
    archive=Path(package['file'])
    require(not archive.is_symlink() and archive.stat().st_size==package['bytes'] and sha(archive)==package['sha256'],'package bytes differ')
    with tarfile.open(fileobj=io.BytesIO(payload(archive)),mode='r:xz') as tar:
     for m in tar:
      seen+=1;expanded+=m.size
      require(seen<=MAX_MEMBERS and 0<=m.size<=MAX_BYTES and expanded<=2*1024**3,'source archive bounds')
      rel=clean(m.name);p=PurePosixPath(rel)
      require(m.isdir() or m.isfile() or m.issym() or m.islnk(),'unsafe member type '+rel)
      chosen=rel.startswith(('usr/bin/','usr/lib/gcc-cross/','usr/libexec/','usr/x86_64-linux-gnu/','usr/lib/x86_64-linux-musl/','usr/include/x86_64-linux-musl/','lib/x86_64-linux-musl/')) or rel=='lib/ld-musl-x86_64.so.1' or (package['package'] in ('libcc1-0','binutils-x86-64-linux-gnu') and rel.startswith('usr/lib/aarch64-linux-gnu/')) or (rel.startswith('usr/share/doc/') and p.name=='copyright')
      if not chosen:continue
      if m.isdir():emit_dir(rel,dest);continue
      emit_dir(posixpath.dirname(rel),dest)
      require(rel not in items,'duplicate compiler input '+rel)
      n=tarfile.TarInfo(rel);n.mode=0o555 if m.mode&0o111 else 0o444
      record={'path':rel,'package':package['package']}
      if m.issym() or m.islnk():
       require(m.size==0,'nonempty link')
       link=m.linkname
       require(link and '\x00' not in link and '\\' not in link and len(link.encode())<=4096,'unsafe link')
       target=posixpath.normpath(link.lstrip('/') if link.startswith('/') else (posixpath.join(str(p.parent),link) if m.issym() else link))
       require(target not in ('','.','..') and not target.startswith(('../','/')),'escaping compiler link')
       target=clean(target)
       if m.issym():
        n.type=tarfile.SYMTYPE;n.linkname=posixpath.relpath(target,str(p.parent));record.update(type='symlink',target=n.linkname,resolved_target=target)
       else:
        n.type=tarfile.LNKTYPE;n.linkname=target;record.update(type='hardlink',target=target,resolved_target=target)
       dest.addfile(n)
      else:
       require(total+m.size<=MAX_BYTES,'compiler input bound')
       # One bounded member, never a Linux-named host file.
       source_file=tar.extractfile(m);require(source_file is not None,'missing member')
       data=source_file.read(m.size+1);require(len(data)==m.size,'short compiler member')
       n.size=len(data);dest.addfile(n,io.BytesIO(data))
       record.update(type='file',bytes=len(data),sha256=hashlib.sha256(data).hexdigest());total+=len(data)
      items[rel]=record
# Resolve every link against the complete exact-case virtual tree, including forward links.
def resolve(path,trail=()):
 require(len(trail)<64 and path not in trail,'link cycle '+path)
 parts=PurePosixPath(path).parts
 for i in range(1,len(parts)+1):
  prefix='/'.join(parts[:i]);entry=items.get(prefix)
  require(entry is not None,'unresolved compiler link '+prefix+' from '+str(trail))
  if entry['type'] in ('symlink','hardlink'):
   require(entry['type']!='hardlink' or i==len(parts),'hardlink parent')
   suffix='/'.join(parts[i:]);target=entry['resolved_target']
   return resolve(posixpath.join(target,suffix) if suffix else target,trail+(path,))
  require(i==len(parts) or entry['type']=='directory','non-directory path component')
 return items[path]
link_failures=[]
for entry in items.values():
 if entry['type'] in ('symlink','hardlink'):
  try:
   resolved=resolve(entry['path'])
   require(entry['type']!='hardlink' or resolved['type']=='file','hardlink not regular')
  except RuntimeError as error:link_failures.append(str(error))
 # No archive write may traverse a symlink, even when the target is internal.
 parent=posixpath.dirname(entry['path'])
 while parent:
  require(items[parent]['type']=='directory','link parent '+parent);parent=posixpath.dirname(parent)
require(not link_failures,'unsafe/unresolved links: '+json.dumps(link_failures))
require(sha(supplement)==supplement_sha,'supplemental lock changed')
require(sha(headers)==headers_sha and sha(lock)=='fbd4eeb24b327cd13e500ef5f44fc57a2fb4f8d3e3688185dabeecd300a64bbc','inputs changed')
manifest=out/'manifest.json';manifest.write_text(json.dumps(sorted(items.values(),key=lambda x:x['path']),indent=2)+'\n')
# Independent archive readback checks every name, kind, byte hash and normalized link.
remaining=dict(items)
with tarfile.open(archive_out,'r:gz') as tar:
 for m in tar:
  e=remaining.pop(m.name,None);require(e is not None,'archive census mismatch')
  if e['type']=='file':require(m.isfile() and m.size==e['bytes'] and hashlib.sha256(tar.extractfile(m).read()).hexdigest()==e['sha256'],'archive byte mismatch')
  elif e['type']=='directory':require(m.isdir(),'archive directory mismatch')
  else:require((m.issym() if e['type']=='symlink' else m.islnk()) and m.linkname==e['target'],'archive link mismatch')
require(not remaining,'archive missing entries')
record={'purpose':'Read-only authenticated ARM-hosted GNU x86_64 cross compiler and GNU/musl sysroots. Linux extraction only; no host install/execution.','archive':str(archive_out),'archive_sha256':sha(archive_out),'archive_bytes':archive_out.stat().st_size,'destination':'/opt/moruna-cross-tools','manifest':str(manifest),'manifest_sha256':sha(manifest),'packages':packages,'entries':len(items),'files':sum(e['type']=='file' for e in items.values()),'bytes':total,'base_image':source.get('image'),'package_lock':str(lock),'package_lock_sha256':sha(lock),'additional_authenticated_headers':str(headers),'additional_authenticated_headers_sha256':headers_sha,'limits':{'members':MAX_MEMBERS,'selected_bytes':MAX_BYTES,'all_member_bytes':2*1024**3},'supplemental_libcc1_lock':str(supplement),'supplemental_libcc1_lock_sha256':supplement_sha,'archive_readback_verified':True,'host_library_selection':{'packages':['binutils-x86-64-linux-gnu','libcc1-0'],'directory':'usr/lib/aarch64-linux-gnu','reason':'Actual ARM cross-as/ld/objdump DT_NEEDED requires same-package bfd/opcodes/ctf host libraries; generic loader dependencies remain provided by unchanged pinned Linux base'}}
for path in (archive_out,manifest):path.chmod(0o400)
bundle=out/'bundle.json';bundle.write_text(json.dumps(record,indent=2)+'\n');bundle.chmod(0o400)
print(json.dumps({'bundle':str(bundle),'archive':str(archive_out),'sha256':record['archive_sha256'],'archive_bytes':record['archive_bytes'],'entries':len(items),'bytes':total,'packages':len(packages)}))
