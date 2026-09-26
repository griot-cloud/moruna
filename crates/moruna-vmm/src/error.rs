//! The monitor's error type and its exit codes.
//!
//! Every failure the monitor can meet is one variant here, and every variant maps to exactly
//! one process exit code, so a host can tell "you configured me wrong" from "the guest broke"
//! from "the hypervisor failed under me" without parsing text.

use moruna_kernel::MorunaError;

/// The guest reported a code and it is relayed as is (codes 0 to 255 other than the ones
/// below come from Moruna inside the guest, MH 4.2).
pub const EXIT_OK: i32 = 0;
/// Bad configuration, an unusable image, or no usable `/dev/kvm`; the diagnostic names the
/// field or the device and the reason (MH 6, "`/dev/kvm` absent").
pub const EXIT_CONFIG: i32 = 2;
/// The guest kernel panicked; the serial log is written to stderr (MH 6).
pub const EXIT_GUEST_PANIC: i32 = 6;
/// The guest stopped (powered off, reset or triple-faulted) without reporting an exit code on
/// the status port. Distinct from every code Moruna itself uses (0, 2, 3, 4, 5, 130).
pub const EXIT_NO_CODE: i32 = 7;
/// The monitor or the hypervisor failed while the guest ran: a `KVM_RUN` error, an internal
/// error exit, a device backend that failed. Distinct from every code Moruna uses.
pub const EXIT_MONITOR: i32 = 8;

/// Every error the monitor returns.
#[derive(thiserror::Error, Debug)]
pub enum VmmError {
    /// A configuration value is invalid; `field` is the CLI flag or config field.
    #[error("config {field}: {msg}")]
    Config {
        /// The flag or field.
        field: &'static str,
        /// What is wrong with it.
        msg: String,
    },
    /// `/dev/kvm` is absent, not permitted, or too old for what the monitor needs.
    #[error("/dev/kvm: {0}")]
    NoKvm(String),
    /// The guest image could not be resolved or failed verification.
    #[error("image {path}: {msg}")]
    Image {
        /// The image path, or the blob inside it.
        path: String,
        /// What is wrong with it.
        msg: String,
    },
    /// An IO operation failed while the guest ran.
    #[error("io {op} {target}: {msg}")]
    Io {
        /// The operation.
        op: &'static str,
        /// The path or socket.
        target: String,
        /// What went wrong.
        msg: String,
    },
    /// A hypervisor call failed.
    #[error("kvm {op}: {msg}")]
    Hypervisor {
        /// The ioctl or step.
        op: &'static str,
        /// What went wrong.
        msg: String,
    },
    /// Guest memory was accessed out of range or a device saw a malformed request that it
    /// cannot answer with a status (the device is then marked broken).
    #[error("device {device}: {msg}")]
    Device {
        /// The device name.
        device: &'static str,
        /// What went wrong.
        msg: String,
    },
    /// A control-socket request was refused; the text is sent back to the client.
    #[error("control: {0}")]
    Control(String),
    /// The requested feature is not available on this architecture or host.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl VmmError {
    /// The process exit code this error ends `moruna-vmm` with.
    pub fn exit_code(&self) -> i32 {
        match self {
            VmmError::Config { .. }
            | VmmError::NoKvm(_)
            | VmmError::Image { .. }
            | VmmError::Unsupported(_) => EXIT_CONFIG,
            VmmError::Io { .. }
            | VmmError::Hypervisor { .. }
            | VmmError::Device { .. }
            | VmmError::Control(_) => EXIT_MONITOR,
        }
    }

    /// Shorthand for a configuration error.
    pub fn config(field: &'static str, msg: impl Into<String>) -> Self {
        VmmError::Config {
            field,
            msg: msg.into(),
        }
    }

    /// Shorthand for an IO error from a `std::io::Error`.
    pub fn io(op: &'static str, target: impl Into<String>, e: &std::io::Error) -> Self {
        VmmError::Io {
            op,
            target: target.into(),
            msg: e.to_string(),
        }
    }

    /// Shorthand for a device error.
    pub fn device(device: &'static str, msg: impl Into<String>) -> Self {
        VmmError::Device {
            device,
            msg: msg.into(),
        }
    }
}

/// The library seam (H-Q7): a caller in the `moruna` crate graph sees the shared error type.
impl From<VmmError> for MorunaError {
    fn from(e: VmmError) -> Self {
        match e {
            VmmError::Config { field, msg } => MorunaError::Config { name: field, msg },
            VmmError::Io { op, target, msg } => MorunaError::Io { op, target, msg },
            VmmError::Unsupported(what) => MorunaError::Config {
                name: "vm",
                msg: format!("unsupported: {what}"),
            },
            other => MorunaError::Io {
                op: "vm",
                target: "moruna-vmm".into(),
                msg: other.to_string(),
            },
        }
    }
}

/// The monitor's result type.
pub type Result<T> = std::result::Result<T, VmmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_t7_every_error_has_its_exit_code() {
        let cases: Vec<(VmmError, i32, &str)> = vec![
            (
                VmmError::config("--memory", "zero"),
                EXIT_CONFIG,
                "config --memory: zero",
            ),
            (
                VmmError::NoKvm("absent".into()),
                EXIT_CONFIG,
                "/dev/kvm: absent",
            ),
            (
                VmmError::Image {
                    path: "p".into(),
                    msg: "m".into(),
                },
                EXIT_CONFIG,
                "image p: m",
            ),
            (
                VmmError::Unsupported("x".into()),
                EXIT_CONFIG,
                "unsupported: x",
            ),
            (
                VmmError::io("read", "f", &std::io::Error::other("e")),
                EXIT_MONITOR,
                "io read f: e",
            ),
            (
                VmmError::Hypervisor {
                    op: "run",
                    msg: "m".into(),
                },
                EXIT_MONITOR,
                "kvm run: m",
            ),
            (VmmError::device("blk", "m"), EXIT_MONITOR, "device blk: m"),
            (VmmError::Control("c".into()), EXIT_MONITOR, "control: c"),
        ];
        for (e, code, text) in cases {
            assert_eq!(e.exit_code(), code, "{e}");
            assert_eq!(e.to_string(), text);
        }
        // Distinct from every code Moruna uses (MH 4.2).
        for c in [EXIT_NO_CODE, EXIT_MONITOR, EXIT_GUEST_PANIC] {
            assert!(![0, 2, 3, 4, 5, 130].contains(&c));
        }
        assert_eq!(EXIT_OK, 0);
    }

    #[test]
    fn vm_t7_errors_cross_into_the_shared_type() {
        let m: MorunaError = VmmError::config("--cpus", "zero").into();
        assert_eq!(m.to_string(), "config --cpus: zero");
        let m: MorunaError = VmmError::Io {
            op: "o",
            target: "t".into(),
            msg: "m".into(),
        }
        .into();
        assert_eq!(m.to_string(), "io o t: m");
        let m: MorunaError = VmmError::Unsupported("u".into()).into();
        assert_eq!(m.to_string(), "config vm: unsupported: u");
        let m: MorunaError = VmmError::NoKvm("gone".into()).into();
        assert_eq!(m.to_string(), "io vm moruna-vmm: /dev/kvm: gone");
    }
}
