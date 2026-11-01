"""Preserve exact immutable source bytes with independent APFS clones, no deletions."""
import ctypes,datetime,hashlib,json,os,shutil,stat,uuid
from pathlib import Path
r=Path(__file__).resolve().parent
old=json.loads((r/'owned-immutable-source-clone-plan.json').read_text());oldroots=set();canonical={}
for x in old['pairs']:
 oldroots.add(Path(x['path']).relative_to(r).parts[0]);p=Path(x['canonical']);canonical.setdefault((x['sha256'],x['mode']),p)
roots=[p/'source' for p in r.iterdir() if p.is_dir() and p.name not in oldroots and p.name.startswith(('source-capture-','combined-inputs-','kernel-inputs-')) and (p/'source').is_dir()]
def sha(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
plan=[]
for source in roots:
 for p in source.rglob('*'):
  z=p.lstat()
  if not stat.S_ISREG(z.st_mode) or z.st_nlink!=1 or z.st_size<4096:continue
  assert z.st_uid==os.getuid() and not p.is_symlink()
  digest=sha(p);key=(digest,stat.S_IMODE(z.st_mode));c=canonical.get(key)
  if c is None:canonical[key]=p;continue
  if p==c:continue
  if c.exists() and c.stat().st_ino!=z.st_ino:
   plan.append({'path':str(p),'canonical':str(c),'sha256':digest,'mode':key[1],'bytes':z.st_size,'device':z.st_dev,'inode':z.st_ino,'atime_ns':z.st_atime_ns,'mtime_ns':z.st_mtime_ns})
record={'purpose':'Independent copy-on-write immutable input sources; retain every path/byte/mode, no mutable code/caches/logs/archives','created_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'roots':[str(p) for p in roots],'pairs':plan,'logical_duplicate_bytes':sum(x['bytes'] for x in plan)}
planpath=r/'owned-new-immutable-source-clone-plan.json';assert not planpath.exists();planpath.write_text(json.dumps(record,indent=2)+'\n')
lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib',use_errno=True);lib.clonefile.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_int];lib.clonefile.restype=ctypes.c_int
result={'plan':str(planpath),'cloned':0,'errors':[],'before_free':shutil.disk_usage(r).free}
for x in plan:
 p=Path(x['path']);c=Path(x['canonical']);z=p.lstat();assert (z.st_dev,z.st_ino,z.st_size)==(x['device'],x['inode'],x['bytes']);assert sha(p)==sha(c)==x['sha256']
 tmp=p.with_name('.codex-cow-'+uuid.uuid4().hex)
 if lib.clonefile(os.fsencode(c),os.fsencode(tmp),0x0008|0x0002):
  result['errors'].append({'path':str(p),'errno':ctypes.get_errno()});break
 os.chmod(tmp,x['mode']);os.utime(tmp,ns=(x['atime_ns'],x['mtime_ns']));assert sha(tmp)==x['sha256'] and tmp.stat().st_ino!=c.stat().st_ino
 os.replace(tmp,p);result['cloned']+=1
result.update(after_free=shutil.disk_usage(r).free,finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat());(r/'owned-new-immutable-source-clone-result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({**result,'logical_duplicate_bytes':record['logical_duplicate_bytes']}));assert not result['errors']
