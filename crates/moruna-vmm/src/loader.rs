//! Loading the guest kernel with `linux-loader`: an ELF `vmlinux` or a bzImage on x86_64, an
//! `Image` on aarch64 (MH 4.8.3). A KVM guest runs the host's architecture, so the loader for
//! the other architecture is not compiled.

use std::fs::File;

use crate::error::{Result, VmmError};
use crate::machine::Machine;

/// Where the kernel landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// The address the first vCPU starts at.
    pub entry: u64,
    /// The first byte after everything the kernel occupies.
    pub kernel_end: u64,
    /// x86_64: the kernel was a bzImage, whose setup header goes into the zero page.
    pub bzimage: bool,
}

fn open(m: &Machine) -> Result<File> {
    File::open(&m.image.kernel).map_err(|e| VmmError::Image {
        path: m.image.kernel.display().to_string(),
        msg: e.to_string(),
    })
}

fn bad_kernel(m: &Machine, e: impl std::fmt::Display) -> VmmError {
    VmmError::Image {
        path: m.image.kernel.display().to_string(),
        msg: format!("not a loadable kernel: {e}"),
    }
}

/// Load the kernel into the machine's memory.
#[cfg(target_arch = "aarch64")]
pub fn load_kernel(m: &Machine) -> Result<Loaded> {
    use linux_loader::loader::{KernelLoader, pe::PE};
    use std::io::{Read, Seek, SeekFrom};
    use vm_memory::GuestAddress;

    let mut f = open(m)?;
    let r = PE::load(
        &m.mem,
        Some(GuestAddress(crate::layout::arm::RAM_START)),
        &mut f,
        None,
    )
    .map_err(|e| bad_kernel(m, e))?;
    // The header's image_size includes the BSS, which the file does not.
    let mut hdr = [0u8; 24];
    f.seek(SeekFrom::Start(0))
        .and_then(|_| f.read_exact(&mut hdr))
        .map_err(|e| bad_kernel(m, e))?;
    let mut sz = [0u8; 8];
    sz.copy_from_slice(&hdr[16..24]);
    let image_size = u64::from_le_bytes(sz);
    let entry = r.kernel_load.0;
    Ok(Loaded {
        entry,
        kernel_end: r.kernel_end.max(entry + image_size),
        bzimage: false,
    })
}

/// Load the kernel into the machine's memory: ELF first, then bzImage.
#[cfg(target_arch = "x86_64")]
pub fn load_kernel(m: &Machine) -> Result<Loaded> {
    use linux_loader::loader::{BzImage, Elf, KernelLoader};
    use vm_memory::GuestAddress;

    use crate::layout::x86;

    let high = Some(GuestAddress(x86::KERNEL_ADDR));
    let mut f = open(m)?;
    if let Ok(r) = Elf::load(&m.mem, None, &mut f, high) {
        return Ok(Loaded {
            entry: r.kernel_load.0,
            kernel_end: r.kernel_end,
            bzimage: false,
        });
    }
    let mut f = open(m)?;
    let r = BzImage::load(&m.mem, None, &mut f, high).map_err(|e| bad_kernel(m, e))?;
    Ok(Loaded {
        entry: r.kernel_load.0 + crate::boot::BZIMAGE_64BIT_ENTRY_OFFSET,
        kernel_end: r.kernel_end,
        bzimage: true,
    })
}

/// Write the Linux zero page (`boot_params`): the loader header, the command line, the
/// initramfs and the e820 map.
#[cfg(target_arch = "x86_64")]
pub fn write_zero_page(m: &Machine, loaded: &Loaded, initrd: Option<(u64, u64)>) -> Result<()> {
    use linux_loader::loader::bootparam::boot_params;
    use vm_memory::{Bytes, GuestAddress};

    use crate::layout::x86;

    let mut p = boot_params::default();
    if loaded.bzimage {
        // The setup header sits at 0x1f1 in the bzImage; copy it as the boot protocol asks.
        let mut raw = vec![0u8; 0x1f1 + std::mem::size_of_val(&p.hdr)];
        use std::io::Read;
        open(m)?
            .read_exact(&mut raw)
            .map_err(|e| bad_kernel(m, e))?;
        // SAFETY: `setup_header` is a plain-old-data C struct of exactly the copied size, and
        // any bit pattern is a valid value of it.
        p.hdr = unsafe { std::ptr::read_unaligned(raw[0x1f1..].as_ptr().cast()) };
    }
    p.hdr.type_of_loader = 0xff;
    p.hdr.boot_flag = 0xaa55;
    p.hdr.header = 0x5372_6448;
    p.hdr.kernel_alignment = 0x0100_0000;
    p.hdr.cmd_line_ptr = x86::CMDLINE_ADDR as u32;
    p.hdr.cmdline_size = m.cmdline.len() as u32 + 1;
    if let Some((addr, size)) = initrd {
        p.hdr.ramdisk_image = addr as u32;
        p.hdr.ramdisk_size = size as u32;
    }
    let map = crate::boot::e820(&m.layout);
    for (i, (addr, size, kind)) in map.iter().enumerate() {
        p.e820_table[i].addr = *addr;
        p.e820_table[i].size = *size;
        p.e820_table[i].r#type = *kind;
    }
    p.e820_entries = map.len() as u8;
    m.mem
        .write_obj(p, GuestAddress(x86::ZERO_PAGE_ADDR))
        .map_err(|e| VmmError::device("boot", format!("zero page: {e}")))
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use crate::layout::{Arch, arm};
    use crate::testing::scratch_dir;
    use vm_memory::{Bytes, GuestAddress};

    fn machine(kernel: &[u8]) -> Machine {
        let dir = scratch_dir("loader");
        let img = dir.join("img");
        std::fs::create_dir_all(&img).unwrap();
        std::fs::write(img.join("Image"), kernel).unwrap();
        let cfg = crate::config::VmConfig {
            image: img.clone(),
            disks: vec![],
            memory_bytes: 256 << 20,
            memory_max_bytes: 256 << 20,
            cpus: 1,
            cpus_max: 1,
            vsock: crate::config::VsockConfig {
                cid: 3,
                uds_path: dir.join("v.sock"),
            },
            control_socket: dir.join("c.sock"),
        };
        let image = crate::image::resolve(&img).unwrap();
        let mut irq = |_| {
            let (_, i) = crate::testing::counting_irq();
            Ok(i)
        };
        Machine::build(
            Arch::Aarch64,
            &cfg,
            image,
            &mut irq,
            Box::new(std::io::sink()),
        )
        .unwrap()
    }

    fn image_bytes(text_offset: u64, image_size: u64) -> Vec<u8> {
        let mut k = vec![0u8; 4096];
        k[8..16].copy_from_slice(&text_offset.to_le_bytes());
        k[16..24].copy_from_slice(&image_size.to_le_bytes());
        k[56..60].copy_from_slice(&0x644d_5241u32.to_le_bytes());
        k[100] = 0xab;
        k
    }

    #[test]
    fn vm_t28_arm64_image_loads_at_its_text_offset() {
        let m = machine(&image_bytes(0, 8 << 20));
        let l = load_kernel(&m).unwrap();
        assert_eq!(l.entry, arm::RAM_START);
        assert_eq!(l.kernel_end, arm::RAM_START + (8 << 20), "BSS counted");
        assert!(!l.bzimage);
        let b: u8 = m.mem.read_obj(GuestAddress(arm::RAM_START + 100)).unwrap();
        assert_eq!(b, 0xab);
        // An image_size of 0 means the legacy 0x80000 text offset.
        let m = machine(&image_bytes(0x1234, 0));
        assert_eq!(load_kernel(&m).unwrap().entry, arm::RAM_START + 0x80000);
    }

    #[test]
    fn vm_t28_bad_kernels_are_refused_naming_the_file() {
        let mut k = image_bytes(0, 0);
        k[56] = 0;
        let m = machine(&k);
        match load_kernel(&m) {
            Err(VmmError::Image { path, msg }) => {
                assert!(path.ends_with("Image") && msg.contains("not a loadable kernel"));
            }
            other => panic!("{other:?}"),
        }
        let m = machine(&[1, 2, 3]);
        assert!(load_kernel(&m).is_err());
        std::fs::remove_file(&m.image.kernel).unwrap();
        assert!(matches!(load_kernel(&m), Err(VmmError::Image { .. })));
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::layout::{Arch, x86};
    use crate::testing::scratch_dir;
    use linux_loader::loader::bootparam::boot_params;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn vm_t28_zero_page_fields() {
        let dir = scratch_dir("zp");
        let img = dir.join("img");
        std::fs::create_dir_all(&img).unwrap();
        std::fs::write(img.join("vmlinux"), b"not a kernel").unwrap();
        let cfg = crate::config::VmConfig::with_defaults(img.clone(), vec![], 256 << 20, 1, 3);
        let mut cfg = cfg;
        cfg.vsock.uds_path = dir.join("v.sock");
        cfg.control_socket = dir.join("c.sock");
        let mut irq = |_| {
            let (_, i) = crate::testing::counting_irq();
            Ok(i)
        };
        let m = Machine::build(
            Arch::X86_64,
            &cfg,
            crate::image::resolve(&img).unwrap(),
            &mut irq,
            Box::new(std::io::sink()),
        )
        .unwrap();
        assert!(load_kernel(&m).is_err());
        let l = Loaded {
            entry: 0x100_0000,
            kernel_end: 0x200_0000,
            bzimage: false,
        };
        write_zero_page(&m, &l, Some((0x300_0000, 0x1000))).unwrap();
        let p: boot_params = m.mem.read_obj(GuestAddress(x86::ZERO_PAGE_ADDR)).unwrap();
        let hdr = p.hdr;
        assert_eq!({ hdr.boot_flag }, 0xaa55);
        assert_eq!({ hdr.cmd_line_ptr }, x86::CMDLINE_ADDR as u32);
        assert_eq!({ hdr.ramdisk_image }, 0x300_0000);
        assert_eq!(p.e820_entries, 2);
    }
}
