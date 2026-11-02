//! `moruna-layer`: an image as the layer files the microkernel's image filesystem
//! serves (SDD 27, f.1).
//!
//! ```text
//! moruna-layer convert LAYOUT REFERENCE OUT   the image's layer files, lowest first
//! moruna-layer derive LAYOUT REFERENCE DIR NAME OUT CMD...
//!                                             the image with DIR's tree as one more
//!                                             layer on top and CMD as its Cmd, as the
//!                                             OCI layout OUT naming it NAME; prints its
//!                                             manifest digest
//! moruna-layer stack LAYOUT REFERENCE OUT FIRST
//!                                             the image's layers as OUT/stack-N.img,
//!                                             their list naming devices blockFIRST
//!                                             onward (OUT/stack.layers), the image's
//!                                             record (OUT/image.record, SDD 30 f.12)
//!                                             and its digest (OUT/image.digest)
//! moruna-layer fixture OUT                    the CI boot's two layers, their list
//!                                             and the check list (feature `testing`)
//! ```

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [_, "convert", layout, reference, out] => convert(layout, reference, out),
        [_, "derive", layout, reference, dir, name, out, cmd @ ..] if !cmd.is_empty() => {
            derive(layout, reference, dir, name, out, cmd)
        }
        [_, "stack", layout, reference, out, first] => stack(layout, reference, out, first),
        #[cfg(feature = "testing")]
        [_, "fixture", out] => fixture(out),
        _ => {
            eprintln!(
                "usage: moruna-layer convert LAYOUT REFERENCE OUT | derive LAYOUT REFERENCE DIR NAME OUT CMD... | stack LAYOUT REFERENCE OUT FIRST | fixture OUT"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("moruna-layer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn convert(layout: &str, reference: &str, out: &str) -> Result<String, moruna_image::Error> {
    let source = moruna_image::OciLayout::open(Path::new(layout))?;
    let converted = moruna_image::layerfile::convert(&source, reference, Path::new(out))?;
    Ok(converted
        .layers
        .iter()
        .map(|l| format!("{} {} pages {}", l.id, l.pages, l.path.display()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn derive(
    layout: &str,
    reference: &str,
    dir: &str,
    name: &str,
    out: &str,
    cmd: &[&str],
) -> Result<String, moruna_image::Error> {
    let source = moruna_image::OciLayout::open(Path::new(layout))?;
    let cmd: Vec<String> = cmd.iter().map(|c| c.to_string()).collect();
    let digest = moruna_image::derive::derive(
        &source,
        reference,
        Path::new(dir),
        &cmd,
        name,
        Path::new(out),
    )?;
    Ok(digest.to_string())
}

/// An image's layers as the disks a boot attaches for it, the list its image filesystem reads
/// (SDD 27 d.4), and the record its identity is checked against on the kernel (SDD 30 f.12).
fn stack(
    layout: &str,
    reference: &str,
    out: &str,
    first: &str,
) -> Result<String, moruna_image::Error> {
    let first: u32 = first
        .parse()
        .map_err(|_| moruna_image::Error::Format(format!("`{first}` is not a device number")))?;
    let out = Path::new(out);
    let fail = |what: String| {
        move |e: std::io::Error| moruna_image::Error::Io {
            what: what.clone(),
            source: e,
        }
    };
    std::fs::create_dir_all(out).map_err(fail(format!("making {}", out.display())))?;
    let source = moruna_image::OciLayout::open(Path::new(layout))?;
    let converted = moruna_image::layerfile::convert(&source, reference, &out.join("converted"))?;
    let mut list = String::from("moruna-layers 1\n");
    let mut lines = Vec::new();
    let mut seals = Vec::new();
    for (i, layer) in converted.layers.iter().enumerate() {
        let to = out.join(format!("stack-{}.img", i + 1));
        std::fs::rename(&layer.path, &to).map_err(fail(format!("moving {}", to.display())))?;
        seals.push(layer.id.bytes());
        list.push_str(&format!(
            "layer block{} {}\n",
            first + i as u32,
            layer.id.hex()
        ));
        lines.push(format!(
            "{} {} pages {}",
            layer.id,
            layer.pages,
            to.display()
        ));
    }
    std::fs::remove_dir_all(out.join("converted")).map_err(fail(format!(
        "removing {}",
        out.join("converted").display()
    )))?;
    let (digest, manifest) = moruna_image::derive::manifest_bytes(&source, reference)?;
    let config = moruna_image::derive::config_bytes(&source, &digest, &manifest)?;
    let mut record = moruna_image::record::Record::new(&manifest, &config, &seals)?.to_bytes();
    // A boot's data file is read up to its first zero byte, in a memory object of whole pages:
    // a record of whole pages ends with a newline, so a zero follows it.
    if record.len() % 4096 == 0 {
        record.push(b'\n');
    }
    for (file, bytes) in [
        ("stack.layers", list.into_bytes()),
        ("image.record", record),
        ("image.digest", format!("{digest}\n").into_bytes()),
    ] {
        let path = out.join(file);
        std::fs::write(&path, bytes).map_err(fail(format!("writing {}", path.display())))?;
        lines.push(path.display().to_string());
    }
    lines.push(format!("image {digest}"));
    Ok(lines.join("\n"))
}

#[cfg(feature = "testing")]
fn fixture(out: &str) -> Result<String, moruna_image::Error> {
    let f = moruna_image::testing::fixture(Path::new(out))?;
    Ok(f.layers
        .iter()
        .map(|(p, id)| format!("{id} {}", p.display()))
        .chain([f.list.display().to_string(), f.checks.display().to_string()])
        .collect::<Vec<_>>()
        .join("\n"))
}
