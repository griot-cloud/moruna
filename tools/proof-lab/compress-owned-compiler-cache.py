"""Compress exact owned unmounted intermediates transparently; preserve all bytes."""
from pathlib import Path
import json,subprocess,hashlib,shutil,os,time,datetime,uuid,stat,sys
from attachments import validate_cache
r=Path(__file__).resolve().parent;lock=Path.home()/'.vm-lock';turn=Path.home()/'.vm-lock-next';assert not turn.exists() or turn.read_text().strip() in {'','moruna'},'reserved foreign turn';cache=json.loads((r/'cache-journals/20a6dd118db319b25150b48a0bac6a13.json').read_text());validate_cache(cache,cache['owner_nonce']);summary_path=Path(sys.argv[1]).resolve(strict=True) if len(sys.argv)>1 else r/'moruna-proof-501-a45a14f0f5ed/summary.json';assert summary_path.is_relative_to(r) and summary_path.is_file() and not summary_path.is_symlink();last=json.loads(summary_path.read_text());assert last.get('cleanup') is True and last.get('cache')==cache,'exact latest cache owner cleanup required'
lock.mkdir(mode=0o700);owner='codex moruna transparent owned compiler compression pid='+str(os.getpid());(lock/'owner').write_text(owner+'\n');identity=(lock.stat().st_dev,lock.stat().st_ino);started=time.monotonic();receipt=r/('owned-compiler-compression-'+uuid.uuid4().hex+'.json');result={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'free_before':shutil.disk_usage(r).free,'cache_journal':str(r/'cache-journals/20a6dd118db319b25150b48a0bac6a13.json'),'files':[],'errors':[],'policy':'Only exact unmounted owned compiler .rmeta/.rlib/.o/.a; preserve decompressed bytes/mode/mtime/owner; independent inode; never source/evidence/final binaries'}
try:
 records=json.loads(subprocess.check_output(['/opt/homebrew/bin/container','list','--all','--format','json'],text=True))
 for e in records:
  c=e.get('configuration') or json.loads(subprocess.check_output(['/opt/homebrew/bin/container','inspect',e['id']],text=True))[0]['configuration'];assert not any(m.get('source')==cache['path'] for m in c.get('mounts',[]))
 paths=[]
 for p in Path(cache['path']).rglob('*'):
  z=p.lstat()
  if stat.S_ISREG(z.st_mode) and z.st_uid==os.getuid() and z.st_nlink==1 and z.st_size>=1024**2 and p.suffix in {'.rmeta','.rlib','.a','.o'} and not z.st_flags&32:paths.append((z.st_size,p))
 for _,p in sorted(paths,reverse=True):
  if time.monotonic()-started>45 or len(result['files'])>=300:break
  assert(lock.stat().st_dev,lock.stat().st_ino)==identity and(lock/'owner').read_text().strip()==owner;assert shutil.disk_usage(r).free>5500*1024**2
  z=p.lstat();digest=hashlib.sha256(p.read_bytes()).hexdigest();tmp=p.with_name('.codex-compress-'+uuid.uuid4().hex)
  subprocess.run(['/usr/bin/ditto','--hfsCompression',str(p),str(tmp)],check=True,timeout=15);v=tmp.stat();assert v.st_size==z.st_size and hashlib.sha256(tmp.read_bytes()).hexdigest()==digest and v.st_uid==z.st_uid
  if v.st_blocks<z.st_blocks:
   os.chmod(tmp,stat.S_IMODE(z.st_mode));os.utime(tmp,ns=(z.st_atime_ns,z.st_mtime_ns));assert(p.stat().st_dev,p.stat().st_ino)==(z.st_dev,z.st_ino);os.replace(tmp,p);result['files'].append({'path':str(p),'sha256':digest,'bytes':z.st_size,'blocks_before':z.st_blocks,'blocks_after':v.st_blocks,'before_inode':z.st_ino,'after_inode':v.st_ino,'mode':stat.S_IMODE(z.st_mode),'mtime_ns':z.st_mtime_ns})
  else:tmp.unlink()
except BaseException as error:
 result['errors'].append(repr(error));raise
finally:
 result.update(free_after=shutil.disk_usage(r).free,elapsed_seconds=round(time.monotonic()-started,2),finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat());receipt.write_text(json.dumps(result,indent=2)+'\n');assert(lock.stat().st_dev,lock.stat().st_ino)==identity and(lock/'owner').read_text().strip()==owner;(lock/'owner').unlink();lock.rmdir();print(json.dumps({'receipt':str(receipt),'compressed_files':len(result['files']),'free_before':result['free_before'],'free_after':result['free_after'],'elapsed_seconds':result['elapsed_seconds'],'errors':result['errors']}))
