#!/usr/bin/env python3
"""Queue one bounded proof fairly; never overwrite another session's reservation."""
import argparse,datetime,importlib.util,json,os,secrets,signal,sys,time
from pathlib import Path
from attachments import create_cache,validate_cache
from run import ROOT,require

def load(name):
    spec=importlib.util.spec_from_file_location('queued_'+name.replace('-','_'),ROOT/(name+'.py'))
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--runner',choices=['std','personality','kernel','native','native-fpu','native-inventory','native-link'],required=True)
    p.add_argument('--phase',default='check');p.add_argument('--inputs',required=True);p.add_argument('--archive',required=True)
    p.add_argument('--origin-proof',metavar='ACTUAL_IMAGE_TOOLS_SUMMARY');p.add_argument('--layers-tests-kernel-inputs')
    p.add_argument('--kd3-python-bundle');p.add_argument('--kd3-gnu-bundle')
    p.add_argument('--tools-bundle');p.add_argument('--cache-journal');p.add_argument('--reuse-summary');p.add_argument('--reuse-transition')
    p.add_argument('--cross-bundle',default=str(ROOT/'cross-tools-archive-1u_y3fit/bundle.json'));p.add_argument('--rust-bundle',default=str(ROOT/'rust-musl-component/musl-target-vgegt4ot/bundle.json'))
    p.add_argument('--seconds',type=int,default=600);p.add_argument('--wait-seconds',type=int,default=1800)
    options=p.parse_args();maximum=1050 if options.runner=='personality' else 1100 if options.runner=='std' and options.phase in ('witness-tests','sdk-proof-build','boot-host-tools','phase-a-tests','kd3-fixture-tools-arm','kd3-fixture-tools-gnu') else 600
    require(60<=options.seconds<=maximum and 60<=options.wait_seconds<=3600,'bounded stage/queue duration required')
    require(not(options.runner=='std' and options.phase=='personality-artifacts'),'use explicit personality runner/cache for musl artifacts')
    require(not options.layers_tests_kernel_inputs or options.runner=='personality','focused layers tests belong only to personality producer')
    require(not options.origin_proof or options.runner=='native-link','optional origin proof belongs only to native-link')
    if options.phase in ('kd3-fixture-tools-arm','kd3-fixture-tools-gnu'):
        require(options.runner=='std' and options.kd3_python_bundle and (not options.phase.endswith('-gnu') or options.kd3_gnu_bundle),'KD3 fixture builds require explicit std producer/Python/GNU inputs')
    os.umask(0o077)
    if options.runner=='personality':
        module=load('combined-linux');helper=module.module;cls=module.PersonalityRun
        options.mode='combined-linux';options.phase='personality-artifacts';key='cargo_lock_sha256';lock='Cargo.lock';toolchain='1.98.1';target='x86_64-unknown-linux-musl';profile='release-debug0'
    elif options.runner=='std':
        module=load('combined-linux');helper=module.module;cls=module.CombinedRun
        options.mode='combined-linux';key='cargo_lock_sha256';lock='Cargo.lock';toolchain='1.98.1';target='x86_64-unknown-linux-gnu' if options.phase=='kd3-fixture-tools-gnu' else 'aarch64-unknown-linux-gnu';profile='native-debug0'
    elif options.runner=='native-link':
        module=load('native-link');helper=module.module;cls=module.NativeLinkRun
        options.mode='native-link';options.phase='link';key='kernel_cargo_lock_sha256';lock='kernel/Cargo.lock';toolchain='nightly-2026-07-21';target='x86_64-moruna-user';profile='build-release0'
    elif options.runner=='native-inventory':
        module=load('native-inventory');helper=module.module;cls=module.NativeInventoryRun
        options.mode='native-inventory';options.phase='inventory';key='kernel_cargo_lock_sha256';lock='kernel/Cargo.lock';toolchain='nightly-2026-07-21';target='x86_64-moruna-user';profile='build-release0'
    elif options.runner in {'native','native-fpu'}:
        is_fpu=options.runner=='native-fpu'
        module=load('native-fpu' if is_fpu else 'native-check');helper=module.module;cls=module.NativeFpuRun if is_fpu else module.NativeRun
        options.mode='native-fpu' if is_fpu else 'native-check';options.phase='fpu' if is_fpu else 'check';key='kernel_cargo_lock_sha256';lock='kernel/Cargo.lock';toolchain='nightly-2026-07-21';target='x86_64-moruna-user' if is_fpu else 'x86_64-unknown-none';profile='build-release0' if is_fpu else 'check-release0'
    else:
        helper=load('ke2-linux');cls=helper.Ke2Run
        options.mode='ke2-linux';key='kernel_cargo_lock_sha256';lock='kernel/Cargo.lock';toolchain='1.98.1';target='aarch64-unknown-linux-gnu';profile='test-debug0'
    inputs=json.loads(helper.private_file(options.inputs).read_text());archive=helper.private_file(options.archive)
    helper.validate_inputs(inputs,archive,lock_path=lock,lock_key=key,max_files=50000 if options.runner in ('std','personality') else 20000,max_bytes=(800 if options.runner in ('std','personality') else 400)*1024**2)
    if options.cache_journal:
        cache=json.loads(helper.private_file(options.cache_journal).read_text());validate_cache(cache,cache['owner_nonce'])
        require(cache['source_lock_sha256']==inputs[key] and cache['inputs_census_sha256']==inputs['inputs_census_sha256'] and cache['toolchain']==toolchain and cache['target']==target and cache['profile']==profile,'exact queued cache input differs')
    else:cache=create_cache(secrets.token_hex(16),inputs[key],toolchain,target,profile,inputs_sha256=inputs['inputs_census_sha256'])
    run=cls(options,inputs,archive,cache);run.archive(str(archive))
    reservation=Path.home()/'.vm-lock-next';journal=ROOT/('queued-turn-'+run.token+'.json');marker=None;acquired=False;executed=False
    def release_marker():
        nonlocal marker
        if marker and reservation.exists():
            info=reservation.lstat()
            if (info.st_dev,info.st_ino,info.st_uid)==(marker['device'],marker['inode'],marker['uid']) and reservation.read_text()=='moruna\n':
                reservation.unlink();marker['removed_after_actual_acquisition']=acquired
                marker['removed_utc']=datetime.datetime.now(datetime.timezone.utc).isoformat();journal.write_text(json.dumps(marker,indent=2)+'\n')
    def interrupted(signum,frame):raise RuntimeError('queued stage interrupted '+str(signum))
    signal.signal(signal.SIGTERM,interrupted);signal.signal(signal.SIGINT,interrupted)
    try:
        end=time.monotonic()+options.wait_seconds
        while True:
            if not reservation.exists() and marker is None:
                try:fd=os.open(reservation,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
                except FileExistsError:pass
                else:
                    with os.fdopen(fd,'w') as output:output.write('moruna\n')
                    info=reservation.lstat();marker={'path':str(reservation),'device':info.st_dev,'inode':info.st_ino,'uid':info.st_uid,'owner_run':run.name,'purpose':options.mode+' '+options.phase,'created_utc':datetime.datetime.now(datetime.timezone.utc).isoformat()};journal.write_text(json.dumps(marker,indent=2)+'\n')
            try:run.acquire();acquired=True;break
            except RuntimeError as error:
                if not str(error).startswith('DEFER:'):raise
                require(time.monotonic()<end,'bounded queue expired; no VM started')
                print('WAIT',str(error),datetime.datetime.now(datetime.timezone.utc).isoformat(),flush=True);time.sleep(min(60,max(1,end-time.monotonic())))
        run.deadline=time.monotonic()+options.seconds;release_marker();print('ACQUIRED',run.name,flush=True)
        executed=True;return run.execute()
    finally:
        release_marker()
        if acquired and not executed:
            # No guest has been started on this narrow pre-execute failure path.
            require(run.fingerprint==(run.lock.stat().st_dev,run.lock.stat().st_ino) and (run.lock/'owner').read_text().strip()==run.owner,'queue lock changed')
            (run.lock/'owner').unlink();run.lock.rmdir()

if __name__=='__main__':sys.exit(main())
