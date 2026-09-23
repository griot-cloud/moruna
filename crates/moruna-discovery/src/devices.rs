//! Device enumeration (e.3, e.5).
//!
//! Without the `cuda` feature there are no devices at all, which is the only shape this crate is
//! built in today: no host in the project has a CUDA driver (preamble E1, no GPU host), so the
//! `cuda` arm below is compiled by nobody yet and is written to the interface d.2 names.

use moruna_kernel::Device;

/// Every accelerator, and a note for each fact worth reporting.
pub(crate) fn enumerate() -> (Vec<Device>, Vec<String>) {
    #[cfg(feature = "cuda")]
    {
        cuda_enumerate()
    }
    #[cfg(not(feature = "cuda"))]
    {
        (
            Vec::new(),
            vec!["built without the cuda feature; no devices enumerated".to_string()],
        )
    }
}

/// Device bytes in use per device, for `Sample.device_used` (e.5).
pub(crate) fn device_used(devices: &[Device]) -> [u64; 8] {
    #[cfg(feature = "cuda")]
    {
        let mut used = [0u64; 8];
        for (slot, device) in used.iter_mut().zip(devices.iter()) {
            if let Ok((free, total)) = cuda_mem_info(device.id) {
                *slot = (total.saturating_sub(free)) as u64;
            }
        }
        used
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = devices;
        [0u64; 8]
    }
}

#[cfg(feature = "cuda")]
fn cuda_enumerate() -> (Vec<Device>, Vec<String>) {
    use moruna_kernel::DeviceId;

    let mut devices = Vec::new();
    let mut notes = Vec::new();
    let count = match cudarc::driver::CudaDevice::count() {
        Ok(count) => count,
        Err(err) => {
            notes.push(format!("no CUDA driver: {err}"));
            return (devices, notes);
        }
    };
    for ordinal in 0..count.min(8) {
        let id = DeviceId(ordinal as u8);
        match cudarc::driver::CudaDevice::new(ordinal as usize) {
            Ok(device) => {
                let (free, total) = device.mem_get_info().unwrap_or((0, 0));
                devices.push(Device {
                    id,
                    total_bytes: total as u64,
                    free_bytes: free as u64,
                    name: device.name().unwrap_or_else(|_| format!("cuda:{ordinal}")),
                });
            }
            Err(err) => notes.push(format!("device {ordinal} could not be opened: {err}")),
        }
    }
    (devices, notes)
}

#[cfg(feature = "cuda")]
fn cuda_mem_info(id: moruna_kernel::DeviceId) -> Result<(usize, usize), String> {
    let device =
        cudarc::driver::CudaDevice::new(id.0 as usize).map_err(|err| format!("device: {err}"))?;
    device.mem_get_info().map_err(|err| format!("mem: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `cuda` there are no devices and the note says why (e.3).
    #[test]
    fn no_devices_without_the_cuda_feature() {
        let (devices, notes) = enumerate();
        if cfg!(feature = "cuda") {
            return;
        }
        assert!(devices.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("cuda"));
        assert_eq!(device_used(&devices), [0u64; 8]);
    }
}
