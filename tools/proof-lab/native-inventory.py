"""Read actual pinned compiler platform macros and existing provider object symbols."""
import importlib.util
from run import ROOT
spec=importlib.util.spec_from_file_location('inventory_native',ROOT/'native-check.py');native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
module=native.module
GUEST=native.GUEST[:native.GUEST.index('unset CARGO_ENCODED_RUSTFLAGS')]+r'''
printf '\nACTUAL_CLANG_ASSEMBLER_MACROS_BEGIN\n'
"$CC_x86_64_unknown_none" --target=x86_64-unknown-none -ffreestanding -nostdlibinc -dM -E -x assembler-with-cpp /dev/null
printf '\nACTUAL_CLANG_RING_TARGET_MACROS_BEGIN\n'
"$CC_x86_64_unknown_none" --target=x86_64-unknown-none -ffreestanding -nostdlibinc \
  -I /proof/source/kernel/vendor/ring-0.17.14/include -include ring-core/target.h -dM -E -x assembler-with-cpp /dev/null
python3 - <<'CODE'
from pathlib import Path
for p in sorted(Path('/proof/target/x86_64-moruna-user/release/build').glob('ring-*/out/*mont*')):
 print('ACTUAL_RING_OBJECT',p,p.stat().st_size)
CODE
for file in /proof/target/x86_64-moruna-user/release/build/ring-*/out/*mont*.o; do
 test ! -f "$file" || readelf --symbols "$file"
done
printf 'MORUNA_NATIVE_INVENTORY_DONE %s %s\n' "$token" "$phase"
'''
class NativeInventoryRun(native.NativeRun):
    marker='MORUNA_NATIVE_INVENTORY_DONE'
    measurement_scope='Actual pinned Clang preprocessor macro inventory and retained ELF object symbol inventory; no build, execution or acceptance proof.'
    runner_files=native.NativeRun.runner_files+('native-inventory.py',)
    def __init__(self,options,inputs,archive,cache):
        super().__init__(options,inputs,archive,cache);self.guest_script=GUEST.replace('__RUST_SRC_LOCK_SHA256__',inputs['stdlib_provenance']['actual_rust_src_lock_sha256'])
