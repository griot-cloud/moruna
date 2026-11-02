//! The vsock device as a vhost-user back end (SDD 30 f.11): what QEMU's
//! `vhost-user-vsock-pci` drives, so a machine has a vsock with no host kernel module. The
//! messages are [`protocol`]'s, tested everywhere; the back end, which shares the guest's
//! memory and the rings' eventfds, is Linux's.

pub mod protocol;

#[cfg(target_os = "linux")]
pub mod backend;
