"""Read-only inventory; never clone, unlink, rename or rewrite cache contents."""
import datetime,hashlib,json,os,stat,subprocess
from collections import defaultdict
from pathlib import Path
from attachments import ROOT,validate_cache
EXCLUDED='20a6dd118db319b25150b48a0bac6a13'
def sha(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def info(p):
 z=p.lstat();return {'path':str(p),'device':z.st_dev,'inode':z.st_ino,'uid':z.st_uid,'gid':z.st_gid,'mode':stat.S_IMODE(z.st_mode),'bytes':z.st_size,'allocated_bytes':z.st_blocks*512,'mtime_ns':z.st_mtime_ns,'atime_ns':z.st_atime_ns,'nlink':z.st_nlink}
containers=json.loads((ROOT/'cache-dedup-container-inspect.json').read_text())
if isinstance(containers,dict):containers=[containers]
mounts=[m['source'] for c in containers for m in c.get('configuration',c).get('mounts',[]) if m.get('source')]
assert not any(Path(m).is_relative_to(ROOT/'caches') for m in mounts),'cache still attached'
uses=defaultdict(list)
for p in ROOT.glob('moruna-proof-501-*/summary.json'):
 z=p.lstat()
 if z.st_uid!=os.getuid() or not stat.S_ISREG(z.st_mode) or p.is_symlink():continue
 v=json.loads(p.read_text());cache=v.get('cache')
 if isinstance(cache,dict) and cache.get('owner_nonce')!=EXCLUDED:
  uses[cache.get('owner_nonce')].append((v.get('started_at',''),p,v))
eligible=[];rejected=[];files=defaultdict(list);scanned=0
for parent in sorted((ROOT/'caches').iterdir()):
 if parent.name==EXCLUDED:continue # Entirely excluded: do not stat or traverse.
 try:
  journal=ROOT/'cache-journals'/(parent.name+'.json');record=json.loads(journal.read_text());validate_cache(record,parent.name)
  prior=sorted(uses[parent.name],key=lambda v:v[0]);assert prior,'no actual run summary'
  latest=prior[-1];assert latest[2].get('cleanup') is True,'latest run cleanup not true'
  # Earlier cache users must also be demonstrably cleaned; no unresolved use adopted.
  assert all(v.get('cleanup') is True for _,_,v in prior),'an earlier use lacks cleanup proof'
  target=Path(record['path']);assert target.is_relative_to(parent)
  eligible.append({'nonce':parent.name,'journal':str(journal),'journal_sha256':sha(journal),'target':str(target),'summary':str(latest[1]),'summary_sha256':sha(latest[1]),'cleanup':True,'prior_uses':len(prior)})
  for base,dirs,names in os.walk(target,followlinks=False):
   dirs[:]=[n for n in dirs if not (Path(base)/n).is_symlink()]
   for n in names:
    p=Path(base)/n;z=p.lstat();scanned+=1
    if not stat.S_ISREG(z.st_mode) or z.st_uid!=os.getuid() or z.st_nlink!=1 or z.st_size<65536:continue
    with p.open('rb') as f:head=f.read(20)
    # Actual linked ELF executable/shared image, not relocatable objects/metadata/archives.
    if len(head)<20 or head[:4]!=b'\x7fELF' or head[5] not in (1,2):continue
    kind=int.from_bytes(head[16:18],'little' if head[5]==1 else 'big')
    if kind not in (2,3):continue
    before=info(p);before['kind']='ELF executable' if kind==2 else 'ELF shared/PIE';before['cache_nonce']=parent.name
    files[z.st_size].append(before)
 except (AssertionError,ValueError,FileNotFoundError) as e:rejected.append({'cache':str(parent),'reason':str(e)})
groups=defaultdict(list)
for size,entries in files.items():
 if len(entries)<2:continue
 for x in entries:
  p=Path(x['path']);x['sha256']=sha(p);assert info(p)['inode']==x['inode'] and p.stat().st_mtime_ns==x['mtime_ns'],'changed while hashing'
  groups[(x['device'],x['sha256'],size)].append(x)
pairs=[]
for key,entries in groups.items():
 if len(entries)<2:continue
 entries.sort(key=lambda x:x['path']);canonical=entries[0]
 for dest in entries[1:]:
  assert canonical['inode']!=dest['inode']
  pairs.append({'before':dest,'canonical':canonical,'estimated_max_reclaim_bytes':dest['allocated_bytes'],'required_clone_flags':10})
record={'purpose':'PLAN ONLY independent-inode APFS CoW dedup of owned inactive linked outputs; no mutation performed','created_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'excluded_cache':EXCLUDED,'container_inspect':str(ROOT/'cache-dedup-container-inspect.json'),'container_inspect_sha256':sha(ROOT/'cache-dedup-container-inspect.json'),'no_cache_mounts':True,'eligible':eligible,'rejected':rejected,'scanned_file_entries':scanned,'pairs':pairs,'logical_duplicate_bytes':sum(x['before']['bytes'] for x in pairs),'estimated_max_reclaim_bytes':sum(x['estimated_max_reclaim_bytes'] for x in pairs),'estimate_warning':'Upper bound only: APFS shared physical extents cannot be established from st_blocks. Actual free-space gain may be smaller or zero. Revalidate all journal/summary/mount/file identities and bytes immediately before any approved cloning; preserve destination mode/uid/gid/mtime/atime/xattrs, independent inode, no hardlink.'}
p=ROOT/'owned-inactive-cache-clone-plan.json';p.write_text(json.dumps(record,indent=2)+'\n');print(json.dumps({'plan':str(p),'sha256':sha(p),'eligible':len(eligible),'rejected':rejected,'pairs':len(pairs),'logical_duplicate_bytes':record['logical_duplicate_bytes'],'estimated_max_reclaim_bytes':record['estimated_max_reclaim_bytes']}))
