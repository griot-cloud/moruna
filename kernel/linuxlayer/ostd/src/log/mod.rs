// SPDX-License-Identifier: MPL-2.0

//! Logging: OSTD's levels, records and macros, written to the core's console until a
//! logger is injected.

mod bridge;
mod level;
mod logger;
mod macros;

use self::bridge::LogCrateBridge;
pub use self::{
    level::{Level, LevelFilter},
    logger::{
        __write_log_record, Log, Record, STATIC_MAX_LEVEL, inject_logger, max_level, set_max_level,
    },
};

/// Starts logging at `level`, and routes the `log` crate's records here.
pub fn init(level: LevelFilter) {
    set_max_level(level);
    static BRIDGE: LogCrateBridge = LogCrateBridge;
    let _ = ::log::set_logger(&BRIDGE);
}
