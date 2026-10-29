//! `moruna-layer`: an image as the layer files the microkernel's image filesystem
//! serves (SDD 27, f.1).
//!
//! ```text
//! moruna-layer convert LAYOUT REFERENCE OUT   the image's layer files, lowest first
//! moruna-layer stack LAYOUT REFERENCE DIR OUT FIRST
//!                                             the image's layers with DIR as one more
//!                                             on top, as OUT/stack-N.img, and their
//!                                             list naming devices blockFIRST onward
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
        [_, "stack", layout, reference, dir, out, first] => {
            stack(layout, reference, dir, out, first)
        }
        #[cfg(feature = "testing")]
        [_, "fixture", out] => fixture(out),
        _ => {
            eprintln!(
                "usage: moruna-layer convert LAYOUT REFERENCE OUT | stack LAYOUT REFERENCE DIR OUT FIRST | fixture OUT"
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

/// An image's layers and a directory's tree above them: the disks a boot attaches for
/// one image, and the list its image filesystem reads (SDD 27 d.4).
fn stack(
    layout: &str,
    reference: &str,
    dir: &str,
    out: &str,
    first: &str,
) -> Result<String, moruna_image::Error> {
    let first: u32 = first
        .parse()
        .map_err(|_| moruna_image::Error::Format(format!("`{first}` is not a device number")))?;
    let out = Path::new(out);
    let source = moruna_image::OciLayout::open(Path::new(layout))?;
    let converted = moruna_image::layerfile::convert(&source, reference, &out.join("converted"))?;
    let top = moruna_image::layerfile::write(
        &moruna_image::layerfile::tree(Path::new(dir))?,
        &out.join("converted").join("top.layer"),
    )?;
    let mut list = String::from("moruna-layers 1\n");
    let mut lines = Vec::new();
    for (i, layer) in converted
        .layers
        .iter()
        .chain(std::iter::once(&top))
        .enumerate()
    {
        let to = out.join(format!("stack-{}.img", i + 1));
        std::fs::rename(&layer.path, &to)
            .map_err(|e| moruna_image::Error::Format(format!("moving {}: {e}", to.display())))?;
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
    let listed = out.join("stack.layers");
    std::fs::write(&listed, list)
        .map_err(|e| moruna_image::Error::Format(format!("writing {}: {e}", listed.display())))?;
    let _ = std::fs::remove_dir_all(out.join("converted"));
    lines.push(listed.display().to_string());
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
