//! DR-T5 (SDD 25, k): configuration space, BAR sizing, the capability walk and MSI-X,
//! against a model function whose BARs decode as hardware does.

use core::cell::RefCell;

use moruna_mmio::Fake;

use super::*;

/// A function's configuration space: plain bytes, except that a BAR register keeps
/// only the bits its size lets it hold, as hardware does.
struct Model {
    bytes: Fake,
    /// The writable mask of each BAR register (zero for an absent one).
    masks: [u32; 6],
    commands: RefCell<Vec<u16>>,
}

impl Model {
    fn new() -> Model {
        let m = Model {
            bytes: Fake::new(CONFIG_SIZE),
            masks: [0; 6],
            commands: RefCell::new(Vec::new()),
        };
        m.bytes.poke(reg::VENDOR, &0x1af4u16.to_le_bytes());
        m.bytes.poke(reg::DEVICE, &0x1042u16.to_le_bytes());
        m.bytes.poke(reg::CLASS, &0x0100_0001u32.to_le_bytes());
        m
    }

    /// BAR `i` at `value` (type bits included) holding `mask`.
    fn bar(mut self, i: usize, value: u32, mask: u32) -> Model {
        self.masks[i] = mask;
        self.bytes.poke(reg::BAR0 + i * 4, &value.to_le_bytes());
        self
    }

    fn caps(self, list: &[(u8, usize)]) -> Model {
        self.bytes
            .poke(reg::STATUS, &STATUS_CAPABILITIES.to_le_bytes());
        self.bytes
            .poke(reg::CAPABILITIES, &[list.first().map_or(0, |c| c.1 as u8)]);
        for (i, (id, at)) in list.iter().enumerate() {
            let next = list.get(i + 1).map_or(0, |c| c.1 as u8);
            self.bytes.poke(*at, &[*id, next]);
        }
        self
    }
}

impl Region for Model {
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn read8(&self, offset: usize) -> Result<u8, Error> {
        self.bytes.read8(offset)
    }
    fn read16(&self, offset: usize) -> Result<u16, Error> {
        self.bytes.read16(offset)
    }
    fn read32(&self, offset: usize) -> Result<u32, Error> {
        self.bytes.read32(offset)
    }
    fn read64(&self, offset: usize) -> Result<u64, Error> {
        self.bytes.read64(offset)
    }
    fn write8(&self, offset: usize, value: u8) -> Result<(), Error> {
        self.bytes.write8(offset, value)
    }
    fn write16(&self, offset: usize, value: u16) -> Result<(), Error> {
        if offset == reg::COMMAND {
            self.commands.borrow_mut().push(value);
        }
        self.bytes.write16(offset, value)
    }
    fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        if (reg::BAR0..reg::BAR0 + 24).contains(&offset) {
            let i = (offset - reg::BAR0) / 4;
            let kept = self.bytes.read32(offset)? & !self.masks[i];
            return self.bytes.write32(offset, kept | (value & self.masks[i]));
        }
        self.bytes.write32(offset, value)
    }
    fn write64(&self, offset: usize, value: u64) -> Result<(), Error> {
        self.bytes.write64(offset, value)
    }
}

#[test]
fn dr_t5_addresses_and_ids() {
    let a = Address {
        bus: 1,
        device: 5,
        function: 2,
    };
    assert_eq!(a.ecam_offset(), (1 << 20) | (5 << 15) | (2 << 12));
    assert_eq!(a.requester_id(), 0x012a);
    assert_eq!(Address::from_ecam_offset(a.ecam_offset()), Some(a));
    assert_eq!(Address::from_ecam_offset(0x800), None);
    assert_eq!(Address::from_ecam_offset(1 << 28), None);
    assert_eq!(alloc::format!("{a}"), "01:05.2");
    assert!(Config::new(Fake::new(CONFIG_SIZE - 1)).is_err());
}

#[test]
fn dr_t5_header_fields() {
    let config = Config::new(Model::new()).expect("config");
    assert_eq!(config.vendor(), Ok(0x1af4));
    assert_eq!(config.device(), Ok(0x1042));
    assert_eq!(config.present(), Ok(true));
    assert_eq!(config.class(), Ok((0x01, 0x00, 0x00)));
    assert_eq!(config.header_type(), Ok(0));
    assert_eq!(config.multifunction(), Ok(false));
    config.region().bytes.poke(reg::HEADER_TYPE, &[0x81]);
    assert_eq!(config.header_type(), Ok(1));
    assert_eq!(config.multifunction(), Ok(true));
    config
        .enable(command::MEMORY | command::BUS_MASTER)
        .expect("enable");
    assert_eq!(config.command(), Ok(0x6));
    config.disable(command::BUS_MASTER).expect("disable");
    assert_eq!(config.command(), Ok(0x2));
    let absent = Config::new(Fake::new(CONFIG_SIZE)).expect("config");
    absent.region().poke(0, &[0xff, 0xff]);
    assert_eq!(absent.present(), Ok(false));
}

#[test]
fn dr_t5_bars_are_sized_with_decoding_off_and_restored() {
    let model = Model::new()
        // BAR 0: 32-bit memory, 4 KiB at 0xfebd_1000.
        .bar(0, 0xfebd_1000, 0xffff_f000)
        // BAR 1: I/O, 32 ports at 0xc040, upper half hardwired to zero.
        .bar(1, 0xc041, 0x0000_ffe0)
        // BAR 2 absent; BARs 4 and 5: 64-bit prefetchable, 16 KiB at 0x8_0000_4000.
        .bar(4, 0x0000_400c, 0xffff_c000)
        .bar(5, 0x8, 0xffff_ffff);
    model.bytes.poke(reg::COMMAND, &0x0007u16.to_le_bytes());
    let config = Config::new(model).expect("config");
    let bars = config.size_bars().expect("sized");
    assert_eq!(
        bars[0],
        Some(Bar::Memory {
            base: 0xfebd_1000,
            size: 0x1000,
            prefetchable: false,
            wide: false
        })
    );
    assert_eq!(
        bars[1],
        Some(Bar::Io {
            base: 0xc040,
            size: 32
        })
    );
    assert_eq!(bars[2], None);
    assert_eq!(bars[3], None);
    assert_eq!(
        bars[4],
        Some(Bar::Memory {
            base: 0x8_0000_4000,
            size: 0x4000,
            prefetchable: true,
            wide: true
        })
    );
    assert_eq!(bars[5], None);
    // Every register is as it was, and decoding was off while it was probed.
    assert_eq!(config.region().read32(reg::BAR0), Ok(0xfebd_1000));
    assert_eq!(config.region().read32(reg::BAR0 + 20), Ok(0x8));
    assert_eq!(*config.region().commands.borrow(), [0x0004, 0x0007]);
    assert_eq!(bars[0].and_then(|b| b.pages()), Some((0xfebd_1000, 0x1000)));
    assert_eq!(bars[1].and_then(|b| b.pages()), None);
    assert_eq!(
        config.bar_base(4),
        Ok(Bar::Memory {
            base: 0x8_0000_4000,
            size: 0,
            prefetchable: true,
            wide: true
        })
    );
    assert_eq!(config.bar_base(6), Err(Error::InvalidArgs));
    let small = Bar::Memory {
        base: 0xfebd_1100,
        size: 0x100,
        prefetchable: false,
        wide: false,
    };
    assert_eq!(small.pages(), Some((0xfebd_1000, 0x1000)));
    let wrapping = Bar::Memory {
        base: u64::MAX - 10,
        size: 0x100,
        prefetchable: false,
        wide: true,
    };
    assert_eq!(wrapping.pages(), None);
}

#[test]
fn dr_t5_a_wide_bar_in_the_last_register_is_refused() {
    let config = Config::new(Model::new().bar(5, 0x4, 0xffff_c000)).expect("config");
    assert_eq!(config.size_bars(), Err(Error::InvalidArgs));
    // The command register is restored even so.
    assert_eq!(config.command(), Ok(0));
}

#[test]
fn dr_t5_capabilities_are_walked_and_bounded() {
    let config =
        Config::new(Model::new().caps(&[(cap::VENDOR, 0x40), (cap::MSIX, 0x98)])).expect("config");
    assert_eq!(
        config.capabilities(),
        Ok(alloc::vec![(cap::VENDOR, 0x40), (cap::MSIX, 0x98)])
    );
    assert_eq!(config.find(cap::MSIX), Ok(Some(0x98)));
    assert_eq!(config.find(cap::MSI), Ok(None));
    let none = Config::new(Model::new()).expect("config");
    assert_eq!(none.capabilities(), Ok(Vec::new()));
    assert_eq!(none.msix(), Ok(None));
    // A list that loops back, and one that points into the header.
    let looped = Config::new(Model::new().caps(&[(cap::VENDOR, 0x40), (cap::VENDOR, 0x50)]))
        .expect("config");
    looped.region().bytes.poke(0x51, &[0x40]);
    assert_eq!(looped.capabilities(), Err(Error::InvalidArgs));
    let header = Config::new(Model::new().caps(&[(cap::VENDOR, 0x40)])).expect("config");
    header.region().bytes.poke(0x41, &[0x10]);
    assert_eq!(header.capabilities(), Err(Error::InvalidArgs));
}

#[test]
fn dr_t5_msix_is_read_programmed_and_enabled() {
    let model = Model::new().caps(&[(cap::MSIX, 0x98)]);
    // Three entries; table in BAR 1 at 0, pending bits in BAR 1 at 0x800; the
    // function mask set as a device comes out of reset.
    model
        .bytes
        .poke(0x9a, &(0x0002u16 | MSIX_FUNCTION_MASK).to_le_bytes());
    model.bytes.poke(0x9c, &0x0000_0001u32.to_le_bytes());
    model.bytes.poke(0xa0, &0x0000_0801u32.to_le_bytes());
    let config = Config::new(model).expect("config");
    let msix = config.msix().expect("read").expect("present");
    assert_eq!(
        msix,
        Msix {
            offset: 0x98,
            entries: 3,
            table_bar: 1,
            table_offset: 0,
            pba_bar: 1,
            pba_offset: 0x800
        }
    );
    let table = Fake::new(4096);
    msix.program(&table, 2, 0xfee0_0000, 0x41).expect("program");
    assert_eq!(table.read32(32), Ok(0xfee0_0000));
    assert_eq!(table.read32(36), Ok(0));
    assert_eq!(table.read32(40), Ok(0x41));
    assert_eq!(table.read32(44), Ok(0));
    assert_eq!(
        msix.program(&table, 3, 0xfee0_0000, 0x41),
        Err(Error::InvalidArgs)
    );
    msix.enable(&config).expect("enable");
    assert_eq!(config.region().read16(0x9a), Ok(MSIX_ENABLE | 0x0002));
}
