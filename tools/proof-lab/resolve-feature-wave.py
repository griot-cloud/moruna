"""Resolve KA3/KA6/flake source integration, preserving independent contracts."""
import json,re,tomllib,ast
from pathlib import Path

root=Path(__file__).resolve().parent
candidate=root/'std-integration-candidate-w2m5oiqs';source=candidate/'source'
pattern=re.compile(r'^<<<<<<<[^\n]*\n(.*?)^=======\n(.*?)^>>>>>>>[^\n]*\n',re.M|re.S)
changed=[]

def resolve(name, choices):
    path=source/name;text=path.read_text();matches=list(pattern.finditer(text));assert len(matches)==len(choices),(name,len(matches))
    n=0
    def take(m):
        nonlocal n
        choice=choices[n];n+=1
        return choice(m[1],m[2])
    path.write_text(pattern.sub(take,text));changed.append(name)

union=lambda a,b:a+b
left=lambda a,b:a
resolve('crates/moruna-job/Cargo.toml',[lambda a,b:b+a])
resolve('crates/moruna-runtime/Cargo.toml',[union])
resolve('crates/moruna-runtime/src/packaged.rs',[left])
resolve('pyproject.toml',[union])
resolve('python/moruna/__init__.py',[union])

usage='''pub const USAGE: &str = "usage: moruna run <spec.json> [--plan <plan.mplan>] [--strict] [--events-json] [--cost-store <dir>] [--cost-model <name>] [--sandbox-python <path>] [--sandbox-cgroup <dir>] [--kinds <kinds.json>]...
       moruna run --vm <spec.json> --image <node-image.json>
       moruna serve --listen <unix:///path | vsock://CID:PORT> [--no-strict] [--cost-store <dir>] [--cost-model <name>] [--sandbox-python <path>] [--sandbox-cgroup <dir>] [--kinds <kinds.json>]...
       moruna kinds [--kinds <kinds.json>]...
       moruna check-job <spec.json> [--kinds <kinds.json>]...
       moruna explain <spec.json> [--planner <name>] [--out <plan.mplan>] [--kinds <kinds.json>]...
       moruna --version

--kinds names a file of the kinds the program running Moruna serves: a JSON array of
declarations, each with `isolation: embedder` and `served` (SDD 18 e.4).";

'''
def cli_usage(a,b):
    return usage+b[b.index('/// The commands that take'):]
def cli_parse(a,b):
    # Keep each owning parser exactly once: declarations first, cost configuration
    # second, command parser last. Native --vm and events/history paths stay intact.
    prefix=b[:b.index('    let command = match parse(')]
    return prefix+a.replace('cost_options(args)','cost_options(&args)')
resolve('crates/moruna-runtime/src/host/cli.rs',[cli_usage,cli_parse])

resolve('architecture/sdd/01-contracts.md',[lambda a,b:a.rstrip('\n')+'\n\n'+
        'KA6 also adds the single type grammar in `moruna-types`, converted by contracts (CT-I16, CT-T12).\n'])
resolve('architecture/sdd/12-python.md',[union,union,union])
resolve('architecture/sdd/22-job.md',[union,union,union])
resolve('architecture/sdd/20-microkernel.md',[
    lambda a,b:a.splitlines(keepends=True)[0]+b.splitlines(keepends=True)[1],
    lambda a,b:b[:b.index('Coverage:')]+a])

jb={**{f'JB-T{i}':f'JB-T{i+1}' for i in range(40,45)},'JB-I15':'JB-I16','JB-I16':'JB-I17'}
py={**{f'PY-T{i}':f'PY-T{i+9}' for i in range(30,35)},'PY-I13':'PY-I14'}
def mapped(text,mapping):
    return re.sub('|'.join(map(re.escape,mapping)),lambda m:mapping[m[0]],text)
def section(text,start,end,mapping):
    a=text.index(start);b=text.index(end,a)
    return text[:a]+mapped(text[a:b],mapping)+text[b:]

p=source/'architecture/sdd/22-job.md';text=p.read_text()
text=section(text,'**JB-I15. A served', '## d. Interfaces',jb)
text=section(text,'**JB-T40 served_declarations','## l. Implementation',jb)
lines=text.splitlines(keepends=True)
for i,line in enumerate(lines):
    if line.startswith('**Status:') or line.startswith('| KA6') or 'moruna_types::ColumnType' in line or '`RefusalCode` gains' in line:
        lines[i]=mapped(line,jb)
p.write_text(''.join(lines))
p=source/'architecture/sdd/12-python.md';text=p.read_text()
text=section(text,'**PY-I13. A kind','## d. Interfaces',py)
text=section(text,'**PY-T30 to PY-T34 served_kinds','## l. Implementation',py)
text=''.join(mapped(line,py) if line.startswith('| SDD 18 KD-I11') else line for line in text.splitlines(keepends=True))
p.write_text(text)
for name in ['crates/moruna-job/tests/served.rs','crates/moruna-runtime/tests/served.rs','architecture/sdd/18-kinds.md']:
    p=source/name;p.write_text(mapped(mapped(p.read_text(),jb),py));changed.append(name)
p=source/'python/tests/test_kinds.py';text=mapped(p.read_text(),py)
text=re.sub(r'test_py_t(3[0-4])',lambda m:'test_py_t'+str(int(m[1])+9),text)
p.write_text(text);changed.append(str(p.relative_to(source)))
p=source/'crates/moruna-job/Cargo.toml';p.write_text(p.read_text().replace('(JB-I15)','(JB-I16)'))
for name in ['Cargo.toml','crates/moruna-job/Cargo.toml','crates/moruna-runtime/Cargo.toml','pyproject.toml']:
    tomllib.loads((source/name).read_text())
for name in ['python/moruna/__init__.py','python/tests/test_kinds.py']:ast.parse((source/name).read_text())
record={'scope':'Private source integration only; not a Git merge or acceptance',
        'resolved':changed,'trace_id_map':{'KA6_job':jb,'KA6_python':py},
        'preserved':['cost parser and current run identity','native descriptor-only VM path','canonical events7/history output',
                     'KA6 served kind parser/declarations','KD2 bound run ports','checkpoint clamp JB-I15/T40',
                     'event record PY-I13/T30-38','current ABI isolation and audited TCG timing bound']}
(candidate/'feature-wave-resolution.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({'resolved':len(changed),'trace_id_map':record['trace_id_map']}))
