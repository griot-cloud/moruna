//! Moruna 1.0 native microkernel launcher: checked artifacts, explicit Linux KVM
//! tools and owned QEMU/vsock lifetimes. Linux-guest monitor internals are test-only.

#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

#[cfg(all(test, target_arch = "x86_64"))]
mod acpi;
#[cfg(all(test, target_arch = "x86_64"))]
mod boot;
pub mod cli;
#[cfg(all(test, target_arch = "x86_64"))]
mod config;
#[cfg(all(test, target_arch = "x86_64"))]
mod control;
#[cfg(any(target_os = "linux", all(test, target_arch = "x86_64")))]
pub mod devices;
pub mod error;
#[cfg(all(test, target_arch = "x86_64"))]
mod fdt;
#[cfg(all(test, target_arch = "x86_64"))]
mod image;
#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod kvm;
#[cfg(all(test, target_arch = "x86_64"))]
mod layout;
mod limits;
#[cfg(all(test, target_arch = "x86_64"))]
mod loader;
#[cfg(all(test, target_arch = "x86_64"))]
mod machine;
pub mod microkernel;
pub mod session;
pub mod sys;
#[cfg(all(test, target_arch = "x86_64"))]
pub(crate) mod testing;
pub mod vhost_user;
pub use session::{Budget, ExitStatus};

/// Boot the guest `config` describes and block until it stops; returns the exit code.
#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
fn boot(config: &config::VmConfig) -> error::Result<i32> {
    kvm::run(config)
}

/// Boot the guest `config` describes: refused on a host without KVM, after the configuration
/// and the image are checked (so a bad configuration is reported as such everywhere).
#[cfg(all(test, not(target_os = "linux"), target_arch = "x86_64"))]
fn boot(config: &config::VmConfig) -> error::Result<i32> {
    config.validate()?;
    image::resolve(&config.image)?;
    Err(error::VmmError::NoKvm(format!(
        "absent: KVM is Linux-only and this monitor was built for {}",
        std::env::consts::OS
    )))
}

#[cfg(all(test, not(target_os = "linux"), target_arch = "x86_64"))]
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
