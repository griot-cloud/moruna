"""Resolve explicit prerequisite/history conflicts for combined compilation."""
from pathlib import Path
import json,re,sys,hashlib,tomllib

c=Path(sys.argv[1]).resolve(strict=True);s=c/'source';r=Path(__file__).resolve().parent
node=r/'source-capture-nodeorigin-mjwj2nig/source'
kb=r/'source-capture-kb5integration-g8jqwr45/source'
records=[]

def write(name,data,reason):
 p=s/name;old=hashlib.sha256(p.read_bytes()).hexdigest();p.write_text(data)
 records.append({'path':name,'before':old,'after':hashlib.sha256(p.read_bytes()).hexdigest(),'reason':reason})

def resolve(name,choose):
 data=(s/name).read_text();count=0
 def f(m):
  nonlocal count
  count+=1
  return choose(count,m[1],m[2])
 data=re.sub(r'^<<<<<<<[^\n]*\n(.*?)^=======\n(.*?)^>>>>>>>[^\n]*\n',f,data,flags=re.M|re.S)
 assert not re.search(r'^<<<<<<<|^=======|^>>>>>>>',data,re.M),name
 write(name,data,'explicit three-way integration of prerequisite contracts')

resolve('crates/moruna-runtime/Cargo.toml',lambda n,a,b:a+b)
def members(n,a,b):
 values=list(dict.fromkeys(re.findall(r'"([^"\n]+)"',a+b)))
 return 'members = ['+', '.join(json.dumps(x) for x in values)+']\n'
resolve('kernel/Cargo.toml',members)
resolve('kernel/linux/server/Cargo.toml',lambda n,a,b:a+b)
resolve('kernel/programs/Cargo.toml',lambda n,a,b:a.replace('moruna-sys.workspace = true',
        'moruna-sys = { workspace = true, features = ["heap"] }')+'\nmoruna-node = { path = "../node/core", version = "=0.1.0" }\n')
resolve('kernel/sys/src/lib.rs',lambda n,a,b:a+b)
resolve('kernel/sys/src/raw.rs',lambda n,a,b:a+b)
resolve('kernel/svc/src/lib.rs',lambda n,a,b:a.replace('Pin, Process,','Pin, PrivateMemory, Process,'))
for name in ['kernel/abi/src/lib.rs','kernel/core/src/account.rs','kernel/core/src/memory.rs','kernel/core/src/syscall.rs']:
 write(name,(node/name).read_text(),'canonical actual owner integration: KB5 charged accounts + KE1 contiguous DMA + ABI2.5 exact immutable origin')

# KB5's new major carries accounts. KD2's image identity method has its own next
# request id instead of colliding with scratch_account(16).
name='kernel/imagefs/protocol/src/lib.rs';data=(kb/name).read_text()
needle='            16 => fn scratch_account(account: Handle) -> ();'
assert data.count(needle)==1
data=data.replace(needle,needle+'\n            /// Actual mounted immutable layer seals, lowest first (SDD30 image identity).\n            17 => fn layers() -> Vec<Bytes>;')
write(name,data,'preserve KB5 account2.0 and KD2 layer identity with distinct request ids')

# Linux task wire account2.0 is authoritative; keep KD2 mounted-layer identity.
name='kernel/linux/protocol/src/lib.rs';data=(kb/name).read_text()
needle='            3 => fn run_granted(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, grant: Grant) -> Ended;'
assert data.count(needle)==1
data=data.replace(needle,needle+'\n            /// Actual mounted immutable layer seals (SDD30 image identity).\n            4 => fn layers() -> Vec<Bytes>;')
write(name,data,'canonical account2.0 with mounted-layer identity; no1.x changed-shape compatibility')

for name in ['Cargo.toml','crates/moruna-runtime/Cargo.toml','kernel/Cargo.toml','kernel/linux/server/Cargo.toml','kernel/programs/Cargo.toml']:
 tomllib.loads((s/name).read_text())
out=c/'foundation-resolution.json';assert not out.exists();out.write_text(json.dumps(records,indent=2)+'\n')
print(json.dumps({'candidate':str(c),'resolved':len(records)}))
