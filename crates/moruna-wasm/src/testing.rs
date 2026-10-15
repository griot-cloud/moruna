//! Components written in the text format, for tests here and in the crates that run packages
//! (feature `testing`): an identity kernel, a refusing one, one that loops, one that grows its
//! memory, a stateful counter, and one that returns fixed bytes.

use std::path::Path;
use std::sync::Arc;

use moruna_package::{Manifest, Package};
use serde_json::json;

/// The manifest of a test component.
pub fn manifest(name: &str, stateful: bool, guard: bool) -> Manifest {
    let mut hints = serde_json::Map::new();
    if stateful {
        hints.insert("stateful".into(), json!(true));
    }
    if !guard {
        hints.insert("memory_guard".into(), json!(false));
    }
    Manifest::from_value(&json!({
        "name": name,
        "entry": "moruna:kernel",
        "declaration": {
            "input_schema": {"n": "int64", "s": "string"},
            "output_schema": {"adds": {}},
            "hints": hints,
        },
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": moruna_package::MORUNA_VERSION,
        "format": "wasm",
        "platforms": ["wasm32"],
    }))
    .unwrap()
}

/// Bytes as a text-format string literal's contents.
pub fn data(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

/// `process` returns its input.
pub const IDENTITY: &str = "
    (i32.store8 (i32.const 16) (i32.const 0))
    (i32.store (i32.const 20) (local.get $ptr))
    (i32.store (i32.const 24) (local.get $len))
    (i32.const 16)";

/// `process` refuses every batch with the error `no`.
pub const REFUSES: &str = "
    (i32.store8 (i32.const 16) (i32.const 1))
    (i32.store (i32.const 20) (i32.const 512))
    (i32.store (i32.const 24) (i32.const 2))
    (i32.const 16)";

/// `process` loops for ever.
pub const LOOPS: &str = "(loop $l (br $l)) (unreachable)";

/// `process` grows its memory until it cannot, then traps.
pub const GROWS: &str = "
    (block $done
      (loop $l
        (br_if $done (i32.eq (memory.grow (i32.const 16)) (i32.const -1)))
        (br $l)))
    (unreachable)";

/// `process` counts its calls in the i64 at 64 and returns its input.
pub const COUNTS: &str = "
    (i64.store (i32.const 64) (i64.add (i64.load (i32.const 64)) (i64.const 1)))
    (i32.store8 (i32.const 16) (i32.const 0))
    (i32.store (i32.const 20) (local.get $ptr))
    (i32.store (i32.const 24) (local.get $len))
    (i32.const 16)";

/// `open` and `close` over the counter at 64.
pub const STATE_FUNCS: &str = r#"
  (func (export "open") (param $ptr i32) (param $len i32) (result i32)
    (if (i32.eq (local.get $len) (i32.const 8))
      (then (i64.store (i32.const 64) (i64.load (local.get $ptr))))
      (else (i64.store (i32.const 64) (i64.const 0))))
    (i32.store8 (i32.const 48) (i32.const 0))
    (i32.const 48))
  (func (export "close") (result i32)
    (i32.store (i32.const 40) (i32.const 64))
    (i32.store (i32.const 44) (i32.const 8))
    (i32.const 40))
"#;

/// The component's `open` and `close`, lifted.
pub const STATE_LIFTS: &str = r#"
  (func (export "open") (param "state" (list u8)) (result (result (error string)))
    (canon lift (core func $i "open") (memory $i "memory") (realloc (func $i "cabi_realloc"))))
  (func (export "close") (result (list u8))
    (canon lift (core func $i "close") (memory $i "memory") (realloc (func $i "cabi_realloc"))))
"#;

/// A component: `declare` answering `declared`, `process` with `body`, and the extras.
pub fn component(
    declared: &str,
    body: &str,
    output: &[u8],
    core_extra: &str,
    lift_extra: &str,
) -> Vec<u8> {
    wat::parse_str(component_text(
        declared, body, output, core_extra, lift_extra,
    ))
    .unwrap()
}

/// The text of [`component`].
pub fn component_text(
    declared: &str,
    body: &str,
    output: &[u8],
    core_extra: &str,
    lift_extra: &str,
) -> String {
    format!(
        r#"(component
  (core module $m
    (memory (export "memory") 2)
    (global $bump (mut i32) (i32.const 65536))
    (data (i32.const 512) "no")
    (data (i32.const 1024) "{manifest}")
    (data (i32.const 32768) "{output}")
    (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
      (local $ptr i32)
      (local.set $ptr (i32.and (i32.add (global.get $bump) (i32.sub (local.get 2) (i32.const 1)))
                               (i32.sub (i32.const 0) (local.get 2))))
      (global.set $bump (i32.add (local.get $ptr) (local.get 3)))
      (block $ok
        (loop $grow
          (br_if $ok (i32.le_u (global.get $bump) (i32.mul (memory.size) (i32.const 65536))))
          (if (i32.eq (memory.grow (i32.const 1)) (i32.const -1)) (then (unreachable)))
          (br $grow)))
      (local.get $ptr))
    (func (export "declare") (result i32)
      (i32.store (i32.const 32) (i32.const 1024))
      (i32.store (i32.const 36) (i32.const {len}))
      (i32.const 32))
    (func (export "process") (param $ptr i32) (param $len i32) (result i32) {body})
    {core_extra}
  )
  (core instance $i (instantiate $m))
  (func (export "declare") (result string)
    (canon lift (core func $i "declare") (memory $i "memory") (realloc (func $i "cabi_realloc"))))
  (func (export "process") (param "batch" (list u8)) (result (result (list u8) (error string)))
    (canon lift (core func $i "process") (memory $i "memory") (realloc (func $i "cabi_realloc"))))
  {lift_extra}
)"#,
        manifest = data(declared.as_bytes()),
        output = data(output),
        len = declared.len(),
    )
}

/// `process` returns `output`, the bytes at 32768.
pub fn constant(len: usize) -> String {
    format!(
        "
    (i32.store8 (i32.const 16) (i32.const 0))
    (i32.store (i32.const 20) (i32.const 32768))
    (i32.store (i32.const 24) (i32.const {len}))
    (i32.const 16)"
    )
}

/// A package from a component and its manifest, at `dir/<name>.wasm`.
pub fn package_at(dir: &Path, bytes: &[u8], manifest: &Manifest) -> Arc<Package> {
    let path = dir.join(format!("{}.wasm", manifest.name));
    let embedded = moruna_package::embed_manifest(bytes, manifest).unwrap();
    std::fs::write(&path, embedded).unwrap();
    Arc::new(Package::open(&path).unwrap())
}
