//! The device descriptor Describe Device writes (`struct gve_device_descriptor`, 40
//! bytes, then its options), checked field by field, and the queue format this driver
//! uses on it.

use alloc::{format, string::String};

use crate::{GveError, PAGE};

/// Bytes of the descriptor before its options, and of an option's header.
pub const HEADER: usize = 40;
pub const OPTION_HEADER: usize = 8;

/// Device option ids (`enum gve_dev_opt_id`).
pub mod option {
    pub const GQI_RAW_ADDRESSING: u16 = 0x1;
    pub const GQI_RDA: u16 = 0x2;
    pub const GQI_QPL: u16 = 0x3;
    pub const DQO_RDA: u16 = 0x4;
    pub const DQO_QPL: u16 = 0x7;
    pub const JUMBO_FRAMES: u16 = 0x8;
}

/// The GQI queue formats (`enum gve_queue_format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Raw DMA addressing: descriptors carry device addresses.
    GqiRda,
    /// Queue page lists: descriptors carry offsets into pages registered first.
    GqiQpl,
}

impl Format {
    /// The value Configure Device Resources carries.
    pub fn code(self) -> u8 {
        match self {
            Format::GqiRda => 0x1,
            Format::GqiQpl => 0x2,
        }
    }
}

/// What the descriptor says that a driver uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    pub max_registered_pages: u64,
    /// Ring sizes, powers of two.
    pub tx_entries: u16,
    pub rx_entries: u16,
    pub default_queues: u16,
    /// The MTU the device runs with.
    pub mtu: u16,
    /// Event counters the device has.
    pub counters: u16,
    pub tx_pages_per_qpl: u16,
    pub mac: u64,
    pub format: Format,
    /// Every option id the device listed, for the driver's line.
    pub options: String,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The largest ring this driver takes: what one contiguous object holds of receive
/// descriptors (64 bytes each).
pub const RING_MAX: u16 = 4096;

impl Description {
    /// Parses the descriptor in `page`, choosing GQI-QPL where the device lists it or
    /// lists no queue format at all (the format every gVNIC began with), GQI-RDA where
    /// it lists only that, and refusing a device that lists only DQO formats.
    pub fn parse(page: &[u8]) -> Result<Description, GveError> {
        let b = page.get(..PAGE).ok_or_else(|| {
            GveError::Malformed(format!("a descriptor page of {} bytes", page.len()))
        })?;
        let total = u16_at(b, 32) as usize;
        if !(HEADER..=PAGE).contains(&total) {
            return Err(GveError::Malformed(format!(
                "a device descriptor of {total} bytes"
            )));
        }
        let ring = |n: u16, what: &str| -> Result<u16, GveError> {
            if n < 2 || !n.is_power_of_two() || n > RING_MAX {
                return Err(GveError::Malformed(format!("{what} rings of {n} entries")));
            }
            Ok(n)
        };
        let tx_entries = ring(u16_at(b, 10), "transmit")?;
        let rx_entries = ring(u16_at(b, 12), "receive")?;
        let mtu = u16_at(b, 16);
        if mtu < 68 {
            return Err(GveError::Malformed(format!("an MTU of {mtu}")));
        }
        let counters = u16_at(b, 18);
        let tx_pages_per_qpl = u16_at(b, 20);
        let mut mac = 0u64;
        for byte in &b[24..30] {
            mac = mac << 8 | *byte as u64;
        }
        if mac == 0 || mac >> 40 & 1 == 1 {
            return Err(GveError::Malformed(format!("the address {mac:#014x}")));
        }
        let count = u16_at(b, 30);
        let mut at = HEADER;
        let (mut gqi_qpl, mut gqi_rda, mut dqo, mut formats) = (false, false, false, false);
        let mut options = String::new();
        for i in 0..count {
            if at + OPTION_HEADER > total {
                return Err(GveError::Malformed(format!(
                    "option {i} of {count} past the descriptor's {total} bytes"
                )));
            }
            let (id, length) = (u16_at(b, at), u16_at(b, at + 2) as usize);
            let end = at + OPTION_HEADER + length;
            if end > total {
                return Err(GveError::Malformed(format!(
                    "option {id:#x} of {length} bytes past the descriptor's {total} bytes"
                )));
            }
            if !options.is_empty() {
                options.push(',');
            }
            options.push_str(&format!("{id:#x}"));
            let required = u32_at(b, at + 4);
            // An option is taken only when the device asks for no feature this driver
            // lacks: every option this driver reads requires none.
            if required == 0 {
                match id {
                    option::GQI_QPL => gqi_qpl = true,
                    option::GQI_RDA | option::GQI_RAW_ADDRESSING => gqi_rda = true,
                    option::DQO_RDA | option::DQO_QPL => dqo = true,
                    _ => {}
                }
            }
            formats |= matches!(
                id,
                option::GQI_QPL
                    | option::GQI_RDA
                    | option::GQI_RAW_ADDRESSING
                    | option::DQO_RDA
                    | option::DQO_QPL
            );
            at = end;
        }
        let format = if gqi_qpl || !formats {
            Format::GqiQpl
        } else if gqi_rda {
            Format::GqiRda
        } else if dqo {
            return Err(GveError::Unsupported(String::from(
                "the device offers only the DQO queue format, which this driver does not build",
            )));
        } else {
            return Err(GveError::Unsupported(format!(
                "the device's queue format options ({options}) all require features this driver lacks"
            )));
        };
        if format == Format::GqiQpl && (tx_pages_per_qpl == 0 || counters == 0) {
            return Err(GveError::Malformed(format!(
                "{tx_pages_per_qpl} transmit pages a queue page list and {counters} counters"
            )));
        }
        if counters == 0 {
            return Err(GveError::Malformed(String::from("no event counters")));
        }
        Ok(Description {
            max_registered_pages: u64::from_be_bytes(b[0..8].try_into().unwrap_or([0; 8])),
            tx_entries,
            rx_entries,
            default_queues: u16_at(b, 14),
            mtu,
            counters,
            tx_pages_per_qpl,
            mac,
            format,
            options,
        })
    }
}
