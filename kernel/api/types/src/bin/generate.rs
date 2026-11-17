//! Regenerates `kernel/api/openapi.json` and `kernel/api/mcp-tools.json` from the types.
//!
//! From `kernel/`: `cargo run -p moruna-api --features schema`.

use std::{fs, io, path::Path};

fn main() -> io::Result<()> {
    let api = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the crate has no parent directory"))?;
    for (name, text) in moruna_api::generate::files() {
        let path = api.join(name);
        fs::write(&path, text)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
