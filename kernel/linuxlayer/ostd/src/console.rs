// SPDX-License-Identifier: MPL-2.0

//! The early console: the core's debug console.

use core::fmt::{self, Write};

struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::backend::debug_write(s.as_bytes());
        Ok(())
    }
}

/// Writes bytes to the console as they are.
pub fn send(bytes: &[u8]) {
    crate::backend::debug_write(bytes);
}

/// Prints to the console.
pub fn early_print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

/// Prints to the console.
#[macro_export]
macro_rules! early_print {
    ($($arg:tt)*) => { $crate::console::early_print(format_args!($($arg)*)) };
}

/// Prints a line to the console.
#[macro_export]
macro_rules! early_println {
    () => { $crate::early_print!("\n") };
    ($($arg:tt)*) => { $crate::early_print!("{}\n", format_args!($($arg)*)) };
}
