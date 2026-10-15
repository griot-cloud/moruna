//! `<package>.cwasm`: a component precompiled for this engine (16 e.6, f.10; 17 f.3).

use std::path::{Path, PathBuf};

use moruna_kernel::{MorunaError, Result};
use moruna_package::{MORUNA_VERSION, Package};
use wasmtime::Engine;
use wasmtime::component::Component;

/// The first sixteen bytes of a `.cwasm` (16 e.6).
pub const MAGIC: [u8; 16] = *b"MORUNA-CWASM\0\0\0\x01";

/// The largest header a reader accepts.
const HEADER_MAX_BYTES: usize = 4096;

/// Where a package's `.cwasm` is: beside it, the extension replaced.
pub fn path_of(package: &Path) -> PathBuf {
    package.with_extension("cwasm")
}

fn header(package: &Package) -> String {
    serde_json::json!({
        "fingerprint": package.fingerprint_string(),
        "moruna": MORUNA_VERSION,
    })
    .to_string()
}

/// The component from `<package>.cwasm` when its header names the package's fingerprint and
/// this Moruna, or the reason it was not used.
pub fn load(engine: &Engine, package: &Package) -> std::result::Result<Component, String> {
    let path = path_of(&package.path);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no {}", path.display()));
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let body = parse(&bytes, package).map_err(|why| format!("{}: {why}", path.display()))?;
    // SAFETY: wasmtime trusts a serialized component's native code. The bytes are the ones
    // `aot` wrote beside the package (16 f.10), whose place is trusted as the package's own tree
    // is (16 PK-I7), and the header ties them to this package's fingerprint and this Moruna's
    // version; wasmtime itself refuses bytes for another engine configuration or version.
    let component = unsafe { Component::deserialize(engine, body) };
    component.map_err(|e| format!("{}: wasmtime refused it: {e}", path.display()))
}

/// The serialized component inside `bytes`, once the header matches.
fn parse<'a>(bytes: &'a [u8], package: &Package) -> std::result::Result<&'a [u8], String> {
    if bytes.len() < 20 || bytes[..16] != MAGIC {
        return Err("not a Moruna .cwasm".into());
    }
    let mut len = [0u8; 4];
    len.copy_from_slice(&bytes[16..20]);
    let len = u32::from_le_bytes(len) as usize;
    if len > HEADER_MAX_BYTES || 20 + len > bytes.len() {
        return Err("its header is cut short".into());
    }
    let found: serde_json::Value = serde_json::from_slice(&bytes[20..20 + len])
        .map_err(|e| format!("its header is not JSON: {e}"))?;
    let want: serde_json::Value =
        serde_json::from_str(&header(package)).map_err(|e| e.to_string())?;
    if found["fingerprint"] != want["fingerprint"] {
        return Err(format!(
            "it was compiled from {} and the package is {}",
            found["fingerprint"], want["fingerprint"]
        ));
    }
    if found["moruna"] != want["moruna"] {
        return Err(format!(
            "it was compiled by Moruna {} and this is {MORUNA_VERSION}",
            found["moruna"]
        ));
    }
    Ok(&bytes[20 + len..])
}

/// `moruna aot <package>.wasm` (16 f.10): compile the component and write `<package>.cwasm`
/// beside it; returns the path.
pub fn aot(package: &Package) -> Result<PathBuf> {
    let engine = crate::engine()?;
    let component = Component::new(engine, package.bytes()).map_err(|e| MorunaError::Config {
        name: "package",
        msg: format!("package: wasm: not a valid component: {e}"),
    })?;
    let body = component.serialize().map_err(|e| MorunaError::Config {
        name: "package",
        msg: format!("package: wasm: serializing the compiled component: {e}"),
    })?;
    let head = header(package);
    let mut out = Vec::with_capacity(20 + head.len() + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(head.len() as u32).to_le_bytes());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(&body);
    let path = path_of(&package.path);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let io = |e: std::io::Error| MorunaError::Io {
        op: "write",
        target: path.display().to_string(),
        msg: e.to_string(),
    };
    std::fs::write(&tmp, &out).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)?;
    Ok(path)
}
