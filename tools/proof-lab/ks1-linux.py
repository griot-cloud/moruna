#!/usr/bin/env python3
"""Specific offline KS1 Linux check; uses the reviewed nonce-owned lifecycle."""
import argparse
import importlib.util
import json
import os
import secrets
import signal
import sys

from attachments import create_cache, validate_cache
from run import ROOT, require

spec = importlib.util.spec_from_file_location('moruna_ke2_runner', ROOT / 'ke2-linux.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

GUEST = r'''set -euo pipefail
token="$1"; phase="$2"; archive_sha="$3"
printf 'MORUNA_KS1_BEGIN %s %s\n' "$token" "$phase"
printf '%s  /tmp/moruna-source.tar\n' "$archive_sha" | sha256sum -c -
mkdir -p /proof/source /proof/cargo
tar -xf /tmp/moruna-source.tar -C /proof/source
cd /proof/source
test -f Cargo.lock
test -f .cargo/config.toml
test -d vendor
export CARGO_HOME=/proof/cargo CARGO_TARGET_DIR=/proof/target
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2
rustup toolchain list | grep -q '^1\.98\.1-aarch64-unknown-linux-gnu'
rustup run 1.98.1 rustc --version
run_cargo() {
  local action="$1"; shift
  rustup run 1.98.1 cargo "$action" --locked --offline --manifest-path Cargo.toml \
    --target aarch64-unknown-linux-gnu --config profile.dev.debug=0 \
    --config profile.dev.incremental=false --config profile.test.debug=0 \
    --config profile.test.incremental=false -j2 "$@"
}
case "$phase" in
  check) run_cargo check -p moruna-storage -p moruna-reactor --all-targets --features moruna-reactor/uring ;;
  tests) run_cargo test -p moruna-storage-format -p moruna-storage -p moruna-reactor --features moruna-reactor/uring -- --test-threads=2 ;;
  clippy) run_cargo clippy -p moruna-storage-format -p moruna-storage -p moruna-reactor --all-targets --features moruna-reactor/uring -- -D warnings ;;
  *) exit 2 ;;
esac
printf 'MORUNA_KS1_DONE %s %s\n' "$token" "$phase"
'''


class Ks1Run(module.Ke2Run):
    guest_script = GUEST
    marker = 'MORUNA_KS1_DONE'
    lock_key = 'cargo_lock_sha256'
    lock_label = 'cargo_lock_sha256'
    runner_files = module.Ke2Run.runner_files + ('ks1-linux.py',)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--phase', choices=('check', 'tests', 'clippy'), default='check')
    parser.add_argument('--inputs', required=True)
    parser.add_argument('--archive', required=True)
    parser.add_argument('--cache-journal')
    parser.add_argument('--seconds', type=int, default=120)
    options = parser.parse_args()
    options.mode = 'ks1-linux'
    require(30 <= options.seconds <= 600, 'KS1 check must be bounded to 30..600 seconds')
    inputs = json.loads(module.private_file(options.inputs).read_text())
    archive = module.private_file(options.archive)
    module.validate_inputs(inputs, archive, lock_path='Cargo.lock', lock_key='cargo_lock_sha256',
                           max_files=50000, max_bytes=800 * 1024 * 1024)
    if not options.run:
        print(json.dumps({'prepared_only': True, 'phase': options.phase, 'source_archive_sha256': inputs['archive_sha256']}))
        return 0
    os.umask(0o077)
    if options.cache_journal:
        cache = json.loads(module.private_file(options.cache_journal).read_text())
        validate_cache(cache, cache['owner_nonce'])
        require(cache['source_lock_sha256'] == inputs['cargo_lock_sha256']
                and cache.get('inputs_census_sha256') == inputs['inputs_census_sha256']
                and cache['toolchain'] == '1.98.1' and cache['target'] == 'aarch64-unknown-linux-gnu'
                and cache['profile'] == 'native-debug0', 'cache differs from exact KS1 inputs')
    else:
        cache = create_cache(secrets.token_hex(16), inputs['cargo_lock_sha256'], '1.98.1',
                             'aarch64-unknown-linux-gnu', 'native-debug0',
                             inputs_sha256=inputs['inputs_census_sha256'])
    run = Ks1Run(options, inputs, archive, cache)
    run.archive(str(archive))
    def interrupted(signum, frame):
        raise RuntimeError(f'interrupted by signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        run.acquire()
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        return 75
    return run.execute()


if __name__ == '__main__':
    sys.exit(main())
