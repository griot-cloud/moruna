//! The x86_64 CPU hot-plug controller: the registers the DSDT's CPU methods read to learn
//! which vCPUs are present and which were just inserted.
//!
//! Every vCPU up to `--cpus-max` is created at boot; those above `--cpus` are described in the
//! MADT as present-but-disabled ("online capable") and report "not present" through `_STA`.
//! `resize --cpus n` marks the next ones enabled and inserting and raises the ACPI Generic
//! Event Device's interrupt; the guest's `_EVT` runs `CSCN`, which walks this controller,
//! `Notify`s each inserting CPU and clears its inserting bit. The guest kernel then adds the
//! CPU and the initramfs's udev rule onlines it (MH 4.8.5, H12). Removal is not offered: MH
//! 4.8.2 is hot-add.
//!
//! Register window: offset 0, `CSEL`, 32 bits, selects a CPU; offset 4, `STAT`, 8 bits: bit 0
//! `CPEN` (the selected CPU is enabled), bit 1 `CINS` (inserted and not yet seen; the guest
//! writes 1 to clear it).

use super::Device;
use crate::error::{Result, VmmError};

/// Offset of `CSEL`.
pub const CSEL: u64 = 0;
/// Offset of `STAT`.
pub const STAT: u64 = 4;
/// `STAT` bit: enabled.
pub const CPEN: u8 = 1;
/// `STAT` bit: inserting.
pub const CINS: u8 = 2;
/// Size of the register window.
pub const WINDOW_BYTES: u64 = 0x10;

/// The controller.
pub struct CpuHotplug {
    selected: u32,
    enabled: Vec<bool>,
    inserting: Vec<bool>,
}

impl CpuHotplug {
    /// `boot` vCPUs enabled out of `max`.
    pub fn new(boot: u32, max: u32) -> Self {
        CpuHotplug {
            selected: 0,
            enabled: (0..max).map(|i| i < boot).collect(),
            inserting: vec![false; max as usize],
        }
    }

    /// vCPUs currently enabled.
    pub fn enabled(&self) -> u32 {
        self.enabled.iter().filter(|e| **e).count() as u32
    }

    /// The most vCPUs this guest can have.
    pub fn max(&self) -> u32 {
        self.enabled.len() as u32
    }

    /// Enable vCPUs up to `target`; returns the ids newly marked inserting (the caller then
    /// raises the GED interrupt if any). Refused above the boot maximum and below the current
    /// count.
    pub fn grow_to(&mut self, target: u32) -> Result<Vec<u32>> {
        if target > self.max() {
            return Err(VmmError::Control(format!(
                "{target} vCPUs exceeds --cpus-max {}",
                self.max()
            )));
        }
        let now = self.enabled();
        if target < now {
            return Err(VmmError::Control(format!(
                "{target} vCPUs is below the {now} online; vCPU removal is not supported (hot-add only)"
            )));
        }
        let mut added = Vec::new();
        for id in now..target {
            self.enabled[id as usize] = true;
            self.inserting[id as usize] = true;
            added.push(id);
        }
        Ok(added)
    }
}

impl Device for CpuHotplug {
    fn name(&self) -> &'static str {
        "cpu-hotplug"
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let i = self.selected as usize;
        match offset {
            CSEL if data.len() == 4 => data.copy_from_slice(&self.selected.to_le_bytes()),
            STAT if !data.is_empty() => {
                let en = self.enabled.get(i).copied().unwrap_or(false);
                let ins = self.inserting.get(i).copied().unwrap_or(false);
                data[0] = if en { CPEN } else { 0 } | if ins { CINS } else { 0 };
            }
            _ => {}
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            CSEL if data.len() == 4 => {
                self.selected = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            }
            STAT if !data.is_empty() => {
                if data[0] & CINS != 0
                    && let Some(b) = self.inserting.get_mut(self.selected as usize)
                {
                    *b = false;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(c: &mut CpuHotplug, id: u32) -> u8 {
        c.write(CSEL, &id.to_le_bytes());
        let mut b = [0u8];
        c.read(STAT, &mut b);
        b[0]
    }

    #[test]
    fn vm_t17_boot_cpus_are_enabled_the_rest_absent() {
        let mut c = CpuHotplug::new(2, 4);
        assert_eq!((c.enabled(), c.max()), (2, 4));
        assert_eq!(stat(&mut c, 0), CPEN);
        assert_eq!(stat(&mut c, 1), CPEN);
        assert_eq!(stat(&mut c, 2), 0);
        assert_eq!(stat(&mut c, 99), 0);
        let mut sel = [0u8; 4];
        c.read(CSEL, &mut sel);
        assert_eq!(u32::from_le_bytes(sel), 99);
        c.read(CSEL, &mut sel[..2]);
        assert_eq!(&sel[..2], &[0, 0]);
        assert_eq!(c.name(), "cpu-hotplug");
    }

    #[test]
    fn vm_t17_hot_add_marks_inserting_until_the_guest_clears_it() {
        let mut c = CpuHotplug::new(1, 4);
        assert_eq!(c.grow_to(3).unwrap(), vec![1, 2]);
        assert_eq!(stat(&mut c, 1), CPEN | CINS);
        assert_eq!(stat(&mut c, 2), CPEN | CINS);
        // The guest's CSCN clears CINS for the selected CPU only.
        c.write(CSEL, &1u32.to_le_bytes());
        c.write(STAT, &[CINS]);
        assert_eq!(stat(&mut c, 1), CPEN);
        assert_eq!(stat(&mut c, 2), CPEN | CINS);
        // Writes that do not clear, or narrow CSEL writes, change nothing.
        c.write(STAT, &[CPEN]);
        c.write(CSEL, &[3]);
        assert_eq!(stat(&mut c, 2), CPEN | CINS);
        c.write(CSEL, &50u32.to_le_bytes());
        c.write(STAT, &[CINS]);
        // Same count again adds nothing; beyond max or below current is refused.
        assert!(c.grow_to(3).unwrap().is_empty());
        assert!(matches!(c.grow_to(5), Err(VmmError::Control(m)) if m.contains("--cpus-max")));
        assert!(matches!(c.grow_to(2), Err(VmmError::Control(m)) if m.contains("removal")));
        assert_eq!(c.enabled(), 3);
    }
}
