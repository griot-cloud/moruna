//! Moruna packaged as a microVM: `moruna-vmm`, the monitor that boots the Moruna guest on
//! KVM (MH 4.8).
//!
//! The monitor boots exactly one guest for exactly one run and exits with the run's exit code.
//! It attaches the block devices it is given, exposes the guest's vsock as Unix sockets, and
//! resizes memory and vCPUs within the maxima given at boot. There is no network device and
//! no way to configure one.
//!
//! Everything but [`kvm`] is portable and tested on any host; [`kvm`] is Linux-only.

#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

pub mod acpi;
pub mod boot;
pub mod cli;
pub mod config;
pub mod control;
pub mod devices;
pub mod error;
pub mod fdt;
pub mod image;
#[cfg(target_os = "linux")]
pub mod kvm;
pub mod layout;
pub mod loader;
pub mod machine;
pub mod session;
pub mod sys;

#[cfg(test)]
pub(crate) mod testing;

pub use session::{Budget, ExitStatus, boot_and_run};

/// Boot the guest `config` describes and block until it stops; returns the exit code.
#[cfg(target_os = "linux")]
pub fn boot(config: &config::VmConfig) -> error::Result<i32> {
    kvm::run(config)
}

/// Boot the guest `config` describes: refused on a host without KVM, after the configuration
/// and the image are checked (so a bad configuration is reported as such everywhere).
#[cfg(not(target_os = "linux"))]
pub fn boot(config: &config::VmConfig) -> error::Result<i32> {
    config.validate()?;
    image::resolve(&config.image)?;
    Err(error::VmmError::NoKvm(format!(
        "absent: KVM is Linux-only and this monitor was built for {}",
        std::env::consts::OS
    )))
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn vm_t30_boot_off_linux_names_the_reason() {
        let dir = testing::scratch_dir("nokvm");
        let img = dir.join("img");
        std::fs::create_dir_all(&img).unwrap();
        std::fs::write(img.join(image::KERNEL_NAMES[0]), b"k").unwrap();
        let mut c = config::VmConfig::with_defaults(img, vec![], 512 << 20, 1, 3);
        c.vsock.uds_path = dir.join("v.sock");
        c.control_socket = dir.join("c.sock");
        let e = boot(&c).unwrap_err();
        assert_eq!(e.exit_code(), error::EXIT_CONFIG);
        assert!(e.to_string().contains("/dev/kvm: absent"), "{e}");
        c.memory_bytes = 1;
        assert!(matches!(boot(&c), Err(error::VmmError::Config { .. })));
    }
}
