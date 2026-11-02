//! Native storage process adapters. Object/auth/cache/journal formats are exclusively
//! moruna-storage-format; transport handles and actual original IO belong to this service.
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
extern crate alloc;
#[cfg(target_os = "none")]
pub mod admission;
#[cfg(target_os = "none")]
pub mod authority;
#[cfg(target_os = "none")]
pub mod delegation;
#[cfg(target_os = "none")]
pub mod device;
#[cfg(target_os = "none")]
pub mod encrypted;
#[cfg(target_os = "none")]
pub mod keys;
#[cfg(target_os = "none")]
pub mod maintenance_config;
#[cfg(target_os = "none")]
pub mod maintenance_server;
#[cfg(target_os = "none")]
pub mod memory;
#[cfg(target_os = "none")]
pub mod namespace;
#[cfg(target_os = "none")]
pub mod namespace_server;
#[cfg(target_os = "none")]
pub mod network;
pub mod protocol;
#[cfg(target_os = "none")]
pub mod proxy;
#[cfg(target_os = "none")]
pub mod session;
#[cfg(target_os = "none")]
pub mod volume;
