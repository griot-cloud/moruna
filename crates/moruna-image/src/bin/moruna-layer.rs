//! `moruna-layer`: an image as the layer files the microkernel's image filesystem
//! serves (SDD 27, f.1).
//!
//! ```text
//! moruna-layer convert LAYOUT REFERENCE OUT   the image's layer files, lowest first
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
        #[cfg(feature = "testing")]
        [_, "fixture", out] => fixture(out),
        _ => {
            eprintln!("usage: moruna-layer convert LAYOUT REFERENCE OUT | fixture OUT");
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
