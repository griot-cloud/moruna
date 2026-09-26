//! The two port-IO devices an x86_64 Linux guest uses to stop the machine: the i8042
//! controller's reset line (`reboot=k`) and the ACPI hardware-reduced sleep and reset
//! registers the FADT points at (`poweroff`). On aarch64 the guest stops through PSCI, which
//! KVM reports as a system event, and neither device exists.
//!
//! Each device records the first stop reason in the shared [`StopSignal`]; the vCPU loop sees
//! the signal and ends the run.

use std::sync::Arc;

use super::{Device, StopReason, StopSignal};

/// The i8042 data port.
pub const I8042_DATA_PORT: u64 = 0x60;
/// The i8042 command/status port.
pub const I8042_COMMAND_PORT: u64 = 0x64;
/// The i8042 command that pulses the CPU reset line.
pub const I8042_RESET_CMD: u8 = 0xfe;
/// The ACPI PM register block: sleep control at +0, sleep status at +1, reset at +2.
pub const ACPI_PM_PORT: u64 = 0x600;
/// Size of the ACPI PM register block.
pub const ACPI_PM_BYTES: u64 = 3;
/// Offset of the sleep control register.
pub const SLEEP_CONTROL: u64 = 0;
/// Offset of the sleep status register.
pub const SLEEP_STATUS: u64 = 1;
/// Offset of the reset register.
pub const RESET_REGISTER: u64 = 2;
/// The value the FADT tells the guest to write to the reset register.
pub const RESET_VALUE: u8 = 0x01;
/// `SLP_EN` in the sleep control register.
pub const SLP_EN: u8 = 1 << 5;
/// The `SLP_TYP` value the DSDT's `_S5` object names (soft off).
pub const S5_TYPE: u8 = 5;

/// The i8042 as far as a guest's reset needs it: status reads 0 (no data, input buffer
/// empty), a reset command stops the run, everything else is ignored.
pub struct I8042 {
    stop: Arc<StopSignal>,
}

impl I8042 {
    /// An i8042 reporting to `stop`.
    pub fn new(stop: Arc<StopSignal>) -> Self {
        I8042 { stop }
    }
}

impl Device for I8042 {
    fn name(&self) -> &'static str {
        "i8042"
    }

    /// Offsets are relative to port 0x60, so 4 is the command port.
    fn read(&mut self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == I8042_COMMAND_PORT - I8042_DATA_PORT && data == [I8042_RESET_CMD] {
            self.stop.request(StopReason::Reset);
        }
    }
}

/// The ACPI hardware-reduced sleep and reset registers.
pub struct AcpiPm {
    stop: Arc<StopSignal>,
}

impl AcpiPm {
    /// Registers reporting to `stop`.
    pub fn new(stop: Arc<StopSignal>) -> Self {
        AcpiPm { stop }
    }
}

impl Device for AcpiPm {
    fn name(&self) -> &'static str {
        "acpi-pm"
    }

    fn read(&mut self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let [v] = data else { return };
        match offset {
            SLEEP_CONTROL if v & SLP_EN != 0 && (v >> 2) & 0x7 == S5_TYPE => {
                self.stop.request(StopReason::PowerOff);
            }
            RESET_REGISTER if *v == RESET_VALUE => {
                self.stop.request(StopReason::Reset);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_t31_i8042_reset_stops_the_run() {
        let stop = Arc::new(StopSignal::default());
        let mut k = I8042::new(stop.clone());
        let mut b = [9u8];
        k.read(4, &mut b);
        assert_eq!(b, [0]);
        k.write(0, &[I8042_RESET_CMD]);
        k.write(4, &[0xaa]);
        assert!(!stop.is_stopped());
        k.write(4, &[I8042_RESET_CMD]);
        assert_eq!(stop.reason(), Some(StopReason::Reset));
        assert_eq!(k.name(), "i8042");
    }

    #[test]
    fn vm_t31_acpi_sleep_and_reset() {
        let stop = Arc::new(StopSignal::default());
        let mut pm = AcpiPm::new(stop.clone());
        let mut b = [9u8];
        pm.read(SLEEP_STATUS, &mut b);
        assert_eq!(b, [0]);
        // S5 without SLP_EN, S3 with it, and a wide write are not a power-off.
        pm.write(SLEEP_CONTROL, &[S5_TYPE << 2]);
        pm.write(SLEEP_CONTROL, &[(3 << 2) | SLP_EN]);
        pm.write(SLEEP_CONTROL, &[(S5_TYPE << 2) | SLP_EN, 0]);
        pm.write(RESET_REGISTER, &[0x02]);
        assert!(!stop.is_stopped());
        pm.write(SLEEP_CONTROL, &[(S5_TYPE << 2) | SLP_EN]);
        assert_eq!(stop.reason(), Some(StopReason::PowerOff));

        let stop = Arc::new(StopSignal::default());
        let mut pm = AcpiPm::new(stop.clone());
        pm.write(RESET_REGISTER, &[RESET_VALUE]);
        assert_eq!(stop.reason(), Some(StopReason::Reset));
        assert_eq!(pm.name(), "acpi-pm");
    }
}
