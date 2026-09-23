//! The machine the generator ran on.
//!
//! Preamble 6.5 and 6.7: a benchmark result names the machine it was measured
//! on, and the board asks the generator's output to name the machine as well as
//! the generator version, because a corpus regenerated on another host is worth
//! knowing about even when the bytes match.

/// A one line description of this host.
pub fn machine() -> String {
    let name = hostname::get()
        .ok()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "unknown-host".to_string());
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!(
        "{name} ({}/{}, {cpus} logical cpus)",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// The banner every run prints first: the generator, its format revision and the
/// machine.
pub fn banner() -> String {
    format!(
        "moruna-bench {} (generator format {}) on {}",
        crate::GENERATOR_VERSION,
        crate::GENERATOR_FORMAT,
        machine()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_line_names_the_os_and_the_architecture() {
        let line = machine();
        assert!(line.contains(std::env::consts::OS), "{line}");
        assert!(line.contains(std::env::consts::ARCH), "{line}");
        assert!(line.contains("logical cpus"), "{line}");
        assert!(!line.starts_with(' '), "{line}");
    }

    #[test]
    fn the_banner_names_the_generator_version_and_the_machine() {
        let line = banner();
        assert!(line.starts_with("moruna-bench "), "{line}");
        assert!(line.contains(crate::GENERATOR_VERSION), "{line}");
        assert!(line.contains("generator format"), "{line}");
        assert!(line.contains(&machine()), "{line}");
    }
}
