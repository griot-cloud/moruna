//! The network server's logic (SDD 31): smoltcp over one or more cards and the
//! loopback, the sockets of `moruna.net`, and the policy every request is checked
//! against. It runs the same on the kernel and on a host, where its tests run two
//! cards joined by a link in memory.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod config;
pub mod device;
mod session;
mod stack;
pub mod tls;

pub use config::{Config, ConfigError, Settings, parse};
pub use device::Port;
pub use session::{Session, Shared, after_turn, factory};
pub use stack::{Card, Iface, Stack};
