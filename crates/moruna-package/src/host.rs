//! Which of a `.pex`'s targets runs here (16 f.7).

use crate::PackageError;
use crate::manifest::parse_target;

/// The target a host offers a `.pex`: `cp3NN-manylinux_<glibc major>_<glibc minor>_<arch>`, from
/// the sandbox interpreter's minor version, its C library's version and the machine. The
/// template reports the three (17 f.9); a host that is not Linux with glibc offers none.
pub fn host_target(
    python_minor: u32,
    glibc: Option<(u32, u32)>,
    arch: &str,
) -> Result<String, PackageError> {
    let Some((major, minor)) = glibc else {
        return Err(PackageError::Unsupported(
            "a .pex runs in the sandbox, which needs Linux with glibc".into(),
        ));
    };
    let target = format!("cp3{python_minor}-manylinux_{major}_{minor}_{arch}");
    if parse_target(&target).is_none() {
        return Err(PackageError::Unsupported(format!(
            "`{arch}` is not an architecture a .pex is built for (x86_64, aarch64)"
        )));
    }
    Ok(target)
}

/// Whether a package's `platform` runs on a host offering `host` (16 f.7): the interpreter tags
/// and the architectures equal, and the platform's glibc no newer than the host's.
pub fn compatible(platform: &str, host: &str) -> bool {
    match (parse_target(platform), parse_target(host)) {
        (Some((p_minor, p_glibc, p_arch)), Some((h_minor, h_glibc, h_arch))) => {
            p_minor == h_minor && p_arch == h_arch && p_glibc <= h_glibc
        }
        _ => false,
    }
}

/// The first of `platforms` that runs on `host`, or a refusal naming both.
pub fn choose<'a>(platforms: &'a [String], host: &str) -> Result<&'a str, PackageError> {
    platforms
        .iter()
        .find(|p| compatible(p, host))
        .map(String::as_str)
        .ok_or_else(|| {
            PackageError::Unsupported(format!(
                "this host offers {host} and the package carries {}",
                platforms.join(", ")
            ))
        })
}
