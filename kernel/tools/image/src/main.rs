//! `moruna-image DESCRIPTION PROGRAMS OUT`: builds the boot image DESCRIPTION names
//! from the programs in the directory PROGRAMS, and writes into OUT the initramfs
//! (`initramfs.cpio`), the boot manifest it carries (`manifest`), and the lines a boot
//! of it must print (`expect`), one per line.

use std::{fs, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, description, programs, out] = args.as_slice() else {
        eprintln!("usage: moruna-image DESCRIPTION PROGRAMS OUT");
        return ExitCode::from(2);
    };
    match run(Path::new(description), Path::new(programs), Path::new(out)) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("moruna-image: {description}: {reason}");
            ExitCode::FAILURE
        }
    }
}

fn run(description: &Path, programs: &Path, out: &Path) -> Result<String, String> {
    let text = fs::read_to_string(description).map_err(|e| e.to_string())?;
    let image = moruna_image::build(&text, programs).map_err(|e| e.to_string())?;
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = out.join(name);
        fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    };
    write("initramfs.cpio", &image.initramfs)?;
    write("manifest", image.manifest.as_bytes())?;
    let mut expect = image.expect.join("\n");
    expect.push('\n');
    write("expect", expect.as_bytes())?;
    Ok(format!(
        "moruna-image: {} bytes of initramfs, {} expected lines, in {}",
        image.initramfs.len(),
        image.expect.len(),
        out.display()
    ))
}
