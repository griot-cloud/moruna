//! A driver's own function (SDD 25, f.8), opened from the handles its supervisor gave
//! it: a device memory resource over its 4 KiB of configuration space (`config`), one
//! over each memory BAR (`bar0` to `bar5`), an MSI resource (`msi`) and a DMA resource
//! (`dma`). The driver can claim nothing else: every claim outside these ranges is
//! refused by the kernel.

use alloc::{format, string::String};

use moruna_svc::{
    DeviceMemory, Handle, Interrupt, Mapping, Msi, ResourceInfo, ResourceKind, map, start,
};

use crate::{CONFIG_SIZE, Config, command};

/// A claimed window: the claim (which keeps another holder from claiming it), its
/// mapping, and the resource's range.
pub struct Claim {
    _memory: DeviceMemory,
    pub info: ResourceInfo,
}

/// One memory BAR: its mapping, and where the BAR's first byte is in it.
pub struct BarWindow {
    pub map: Mapping<'static>,
    pub delta: u64,
}

pub struct Function {
    pub config: Config<Mapping<'static>>,
    claims: alloc::vec::Vec<Claim>,
    bars: [Option<BarWindow>; 6],
    msi: Option<(Handle, ResourceInfo)>,
    dma: Option<Handle>,
}

fn claim(slot: &str, kind: ResourceKind) -> Result<Option<(Mapping<'static>, Claim)>, String> {
    let Some(resource) = start::take(slot) else {
        return Ok(None);
    };
    let info = resource
        .resource_info()
        .map_err(|e| format!("{slot}: resource_info: {}", e.name()))?;
    if info.kind != kind {
        return Err(format!("{slot}: a {:?} resource, not {kind:?}", info.kind));
    }
    let memory = DeviceMemory::create(&resource, info.base, info.len).map_err(|e| {
        format!(
            "{slot}: claiming {:#x}+{:#x}: {}",
            info.base,
            info.len,
            e.name()
        )
    })?;
    let mapping = memory
        .map_detached(0, info.len, map::WRITE)
        .map_err(|e| format!("{slot}: mapping: {}", e.name()))?;
    Ok(Some((
        mapping,
        Claim {
            _memory: memory,
            info,
        },
    )))
}

impl Function {
    /// Claims and maps the function's configuration space and memory BARs, and takes
    /// its MSI and DMA resources.
    pub fn open() -> Result<Function, String> {
        let (config_map, config_claim) = claim("config", ResourceKind::DeviceMemory)?
            .ok_or("the supervisor gave no `config` window")?;
        if config_claim.info.len < CONFIG_SIZE as u64 {
            return Err(String::from("the `config` window is smaller than 4 KiB"));
        }
        let config = Config::new(config_map).map_err(|e| format!("config: {}", e.name()))?;
        let mut claims = alloc::vec![config_claim];
        let mut bars: [Option<BarWindow>; 6] = Default::default();
        for (i, bar) in bars.iter_mut().enumerate() {
            let Some((map, claim)) = claim(&format!("bar{i}"), ResourceKind::DeviceMemory)? else {
                continue;
            };
            let base = match config.bar_base(i) {
                Ok(crate::Bar::Memory { base, .. }) => base,
                Ok(_) => return Err(format!("bar{i} is not a memory BAR")),
                Err(e) => return Err(format!("bar{i}: {}", e.name())),
            };
            let delta = base
                .checked_sub(claim.info.base)
                .filter(|d| *d < claim.info.len)
                .ok_or_else(|| format!("bar{i} at {base:#x} is outside its window"))?;
            *bar = Some(BarWindow { map, delta });
            claims.push(claim);
        }
        let msi = match start::take("msi") {
            Some(h) => {
                let info = h
                    .resource_info()
                    .map_err(|e| format!("msi: resource_info: {}", e.name()))?;
                Some((h, info))
            }
            None => None,
        };
        Ok(Function {
            config,
            claims,
            bars,
            msi,
            dma: start::take("dma"),
        })
    }

    /// The physical address of the configuration window, which names the function.
    pub fn config_base(&self) -> u64 {
        self.claims.first().map_or(0, |c| c.info.base)
    }

    pub fn bar(&self, index: usize) -> Option<&BarWindow> {
        self.bars.get(index).and_then(Option::as_ref)
    }

    /// Takes the BAR windows out, each with the offset of its BAR's first byte.
    pub fn take_bars(&mut self) -> [Option<(Mapping<'static>, u64)>; 6] {
        core::array::from_fn(|i| self.bars[i].take().map(|b| (b.map, b.delta)))
    }

    /// How many interrupts the supervisor gave the function.
    pub fn interrupts(&self) -> u64 {
        self.msi.as_ref().map_or(0, |(_, info)| info.len)
    }

    /// The function's interrupt `index`, and the message that raises it.
    pub fn interrupt(&self, index: u64) -> Result<(Interrupt, Msi), String> {
        let (resource, info) = self.msi.as_ref().ok_or("the supervisor gave no `msi`")?;
        Interrupt::create_msi(resource, info.base + index)
            .map_err(|e| format!("interrupt {index}: {}", e.name()))
    }

    /// Takes the DMA resource, without which the function's device can be given no
    /// memory.
    pub fn take_dma(&mut self) -> Result<Handle, String> {
        self.dma
            .take()
            .ok_or_else(|| String::from("the supervisor gave no `dma`"))
    }

    /// Lets the device decode its memory BARs and master the bus, with legacy INTx
    /// off: the last step before a device may move data.
    pub fn enable_bus_master(&self) -> Result<(), String> {
        self.config
            .enable(command::MEMORY | command::BUS_MASTER | command::INTX_DISABLE)
            .map_err(|e| format!("command register: {}", e.name()))
    }
}
