"""Move an unmounted owned compiler cache to a new source census; force local rebuild."""
import datetime,json,os,pathlib,subprocess,sys,tomllib
from attachments import ROOT,private_directory,validate_cache
from run import require,sha

old_journal=pathlib.Path(sys.argv[1]).resolve(strict=True)
new_provenance=pathlib.Path(sys.argv[2]).resolve(strict=True)
old=json.loads(old_journal.read_text());validate_cache(old,old['owner_nonce'])
require(old_journal==ROOT/'cache-journals'/(old['owner_nonce']+'.json'),'wrong authoritative cache journal')
new=json.loads(new_provenance.read_text());new_source=new_provenance.parent/'source'
lock_key='kernel_cargo_lock_sha256' if 'kernel_cargo_lock_sha256' in new else 'cargo_lock_sha256'
lock_path='kernel/Cargo.lock' if lock_key=='kernel_cargo_lock_sha256' else 'Cargo.lock'
require(sha(new_source/lock_path)==new[lock_key],'new lock differs')
prior_provenance_path=pathlib.Path(sys.argv[3]).resolve(strict=True) if len(sys.argv)>3 else ROOT/'combined-inputs-u21osq59/provenance.json'
prior=prior_provenance_path.parent; prior_provenance=json.loads(prior_provenance_path.read_text())
require(prior_provenance['inputs_census_sha256']==old['inputs_census_sha256'],'old cache source binding differs')
def pins(path):
    return sorted((p['name'],p['version'],p.get('source'),p.get('checksum')) for p in tomllib.loads(path.read_text())['package'] if p.get('source'))
require(pins(prior/'source'/lock_path)==pins(new_source/lock_path),'external source pins changed')
require(sha(prior/'source/.cargo/config.toml')==sha(new_source/'.cargo/config.toml'),'compiler/source policy changed')
last_summary=pathlib.Path(sys.argv[4]).resolve(strict=True) if len(sys.argv)>4 else ROOT/'moruna-proof-501-6011ed5c9cb3/summary.json'
last=json.loads(last_summary.read_text())
require(last['cleanup'] is True,'previous actual cache owner not confirmed cleaned')
# A prepared source may have been retargeted but never executed. Walk only exact
# recorded namespace transitions back to the last real owner cleanup; do not
# fabricate a run summary for a source that has never run.
ancestor=old; transition_chain=[]
while ancestor != last['cache']:
    require(len(transition_chain)<16,'cache transition ancestry bound')
    link=pathlib.Path(ancestor.get('previous_binding_receipt','')).resolve(strict=True)
    require(link.parent==ROOT and link.name.startswith('cache-retarget-'),'transition receipt outside proof root')
    info=link.lstat();require(link.is_file() and not link.is_symlink() and info.st_uid==os.getuid() and info.st_nlink==1,'transition receipt ownership differs')
    step=json.loads(link.read_text())
    require(step.get('moved') is True and step.get('to')==ancestor,'transition does not prove exact current binding')
    prior_binding=step['from']
    require(all(prior_binding[k]==old[k] for k in ('owner_nonce','device','inode','toolchain','target','profile','destination','readonly')),'transition changed cache ownership or compiler policy')
    transition_chain.append({'path':str(link),'sha256':sha(link)})
    ancestor=prior_binding
require(ancestor==last['cache'],'previous exact cache owner not confirmed cleaned')
records=json.loads(subprocess.check_output(['/opt/homebrew/bin/container','list','--all','--format','json'],text=True))
for entry in records:
    config=entry.get('configuration')
    if config is None:
        config=json.loads(subprocess.check_output(['/opt/homebrew/bin/container','inspect',entry['id']],text=True))[0]['configuration']
    require(not any(m.get('source')==old['path'] for m in config.get('mounts',[])), 'old cache still mounted')
parent=ROOT/'caches'/old['owner_nonce'];private_directory(parent)
destination=parent/new[lock_key]/new['inputs_census_sha256']/old['toolchain']/old['target']/old['profile']/'target'
require(not destination.exists(),'new cache namespace already exists')
destination.parent.mkdir(mode=0o700,parents=True,exist_ok=True);private_directory(destination.parent)
record={'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'from':old,
        'prior_provenance':str(prior_provenance_path),'prior_provenance_sha256':sha(prior_provenance_path),
        'new_provenance':str(new_provenance),'new_provenance_sha256':sha(new_provenance),
        'last_actual_cleanup_summary':str(last_summary),'last_actual_cleanup_summary_sha256':sha(last_summary),'prepared_transition_chain':transition_chain,
        'policy':'Unchanged exact external packages/toolchain/target/profile/config; ALL authored package artifacts MUST be cleaned before compilation to defeat deterministic archive mtime freshness.',
        'moved':False}
receipt=ROOT/('cache-retarget-'+old['owner_nonce']+'-'+new['inputs_census_sha256'][:12]+'.json');require(not receipt.exists(),'transition already exists')
receipt.write_text(json.dumps(record,indent=2)+'\n')
os.rename(old['path'],destination)
current={**old,'path':str(destination),'source_lock_sha256':new[lock_key],
         'inputs_census_sha256':new['inputs_census_sha256'],'requires_local_clean':True,
         'previous_binding_receipt':str(receipt)}
require((destination.stat().st_dev,destination.stat().st_ino)==(old['device'],old['inode']),'moved cache identity changed')
prior_journal=old_journal.with_name(old['owner_nonce']+'-previous-'+old['inputs_census_sha256'][:12]+'.json');require(not prior_journal.exists(),'prior journal collision')
prior_journal.write_text(json.dumps(old,indent=2)+'\n')
temporary=old_journal.with_suffix('.new');require(not temporary.exists(),'journal update collision')
temporary.write_text(json.dumps(current,indent=2)+'\n');os.replace(temporary,old_journal)
record.update(moved=True,to=current);receipt.write_text(json.dumps(record,indent=2)+'\n');validate_cache(current,current['owner_nonce'])
print(json.dumps({'cache_journal':str(old_journal),'new_path':str(destination),'requires_local_clean':True}))
