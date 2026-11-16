// SPDX-License-Identifier: MPL-2.0

// User space: there is no ACPI reset port to drive; powering off ends the personality's
// process (`ostd::arch::power::try_poweroff`).

crate::register_poweroff_handler!(
    ostd::arch::power::try_poweroff,
    crate::power::Priority::HIGH
);

pub(super) fn init() {}
