//! Thread-local storage (system design 5B): ELF TLS, x86_64 psABI variant II, for std and the C
//! library alike, and the one control block each thread has.
//!
//! A program built by `kernel/tools/build-std.sh` carries a `PT_TLS` template, which the kernel's
//! loader maps with the program and the linker script (`kernel/targets/x86_64-moruna.ld`) makes
//! readable through the program's own headers at `__ehdr_start`. Each thread gets one block:
//! its TLS, the template's image copied and the rest zeroed, directly below its [`Control`],
//! whose address is the thread pointer. The thread's FS base holds it, and the control block's
//! first word holds it again (FS:0), so code reads a thread-local at `fs:[-offset]` with no call.
//!
//! The first thread's block is made by [`super::start`], before `main`; every other thread's by
//! [`super::thread::spawn`], before its code runs, and it is freed when the thread ends.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::mem::{align_of, offset_of, size_of};
use core::ptr;

use super::os;

/// The bytes the control block keeps for the C library's own per-thread state (relibc's `Tcb`
/// after its `GenericTcb`), zeroed when the thread starts. Its layout is the C library's.
pub const LIBC_AREA: usize = 512;

/// One thread's control block: at the thread pointer, its TLS directly below.
///
/// It begins as relibc's `GenericTcb` does (`os_specific` is empty on Moruna), so the C
/// library reads this block as its own `Tcb`: the end of the TLS, which is this block's
/// address, its length, this block's address again and this block's length. The C library's
/// remaining fields fill [`Control::libc`]; the runtime's own come after.
#[repr(C, align(64))]
pub struct Control {
    /// The end of the thread's static TLS: this block's own address, which FS:0 reads.
    pub tls_end: *mut u8,
    /// The TLS's length in bytes, from its start to `tls_end`.
    pub tls_len: usize,
    /// This block.
    pub tcb_ptr: *mut Control,
    /// This block's length in bytes.
    pub tcb_len: usize,
    /// The C library's per-thread state, its layout its own.
    pub libc: [u8; LIBC_AREA],
    /// The allocation this block and its TLS were made in, freed at the thread's end.
    base: *mut u8,
    size: usize,
    align: usize,
}

const _: () = assert!(offset_of!(Control, tls_end) == 0);
const _: () = assert!(offset_of!(Control, tcb_ptr) == 0x10);
const _: () = assert!(offset_of!(Control, libc) == 0x20);
const _: () = assert!(offset_of!(Control, base) == 0x20 + LIBC_AREA);

/// A program's TLS template (`PT_TLS`): its initial image, and each thread's block of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Template {
    /// The address of the initial image.
    pub image: usize,
    /// The image's length: the initialized thread-locals.
    pub image_len: usize,
    /// A thread's TLS, the zeroed ones after the image included.
    pub size: usize,
    /// The TLS's alignment, which the thread pointer has too.
    pub align: usize,
}

const ELF_MAGIC: [u8; 4] = *b"\x7fELF";
const PT_TLS: u32 = 7;

fn word<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

impl Template {
    /// None: a program with no thread-locals.
    pub const NONE: Template = Template {
        image: 0,
        image_len: 0,
        size: 0,
        align: 1,
    };

    /// The `PT_TLS` program header of an ELF64 image whose headers are `headers` (the ELF
    /// header and the program headers it names), or `None` for an image without one or headers
    /// that are not whole.
    pub fn from_headers(headers: &[u8]) -> Option<Template> {
        if word::<4>(headers, 0)? != ELF_MAGIC {
            return None;
        }
        let phoff = u64::from_le_bytes(word(headers, 32)?) as usize;
        let phentsize = u16::from_le_bytes(word(headers, 54)?) as usize;
        let phnum = u16::from_le_bytes(word(headers, 56)?) as usize;
        (0..phnum).find_map(|i| {
            let at = phoff.checked_add(i.checked_mul(phentsize)?)?;
            if u32::from_le_bytes(word(headers, at)?) != PT_TLS {
                return None;
            }
            let field = |o: usize| word::<8>(headers, at + o).map(u64::from_le_bytes);
            Some(Template {
                image: field(16)? as usize,
                image_len: field(32)? as usize,
                size: field(40)? as usize,
                align: (field(48)? as usize).max(1),
            })
        })
    }

    /// The block a thread needs: its layout, and the thread pointer's offset in it. The TLS
    /// starts `size` rounded up to `align` below the thread pointer, as the linker computes each
    /// thread-local's offset, and the pointer is aligned to both the TLS and the control block.
    pub fn block(&self) -> Option<(Layout, usize)> {
        let align = self.align.max(align_of::<Control>());
        let tls = self.size.checked_next_multiple_of(align)?;
        let layout = Layout::from_size_align(tls.checked_add(size_of::<Control>())?, align).ok()?;
        Some((layout, tls))
    }
}

/// A thread's block, made for it and not yet installed.
pub(crate) struct Block {
    control: *mut Control,
}

// SAFETY: a block is made by one thread for another, which owns it from its start.
unsafe impl Send for Block {}

impl Block {
    /// A block for `template`: the image copied, the rest of the TLS and the control block
    /// zeroed, the control block's own fields set. `None` when the memory is not there.
    pub(crate) fn new(template: &Template) -> Option<Block> {
        let (layout, tp) = template.block()?;
        // SAFETY: the layout has a control block's size at least.
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            return None;
        }
        // SAFETY: `tp` lies inside the allocation, with a whole control block after it.
        let control = unsafe { base.add(tp) }.cast::<Control>();
        let tls_len = template.size.next_multiple_of(template.align);
        // SAFETY: the TLS lies in the allocation (`tls_len <= tp`), and the image is the
        // template's initial bytes, which the loader mapped readable with the program.
        unsafe {
            let tls = control.cast::<u8>().sub(tls_len);
            ptr::copy_nonoverlapping(template.image as *const u8, tls, template.image_len);
            let control_ref = &mut *control;
            control_ref.tls_end = control.cast();
            control_ref.tls_len = tls_len;
            control_ref.tcb_ptr = control;
            control_ref.tcb_len = size_of::<Control>();
            control_ref.base = base;
            control_ref.size = layout.size();
            control_ref.align = layout.align();
        }
        Some(Block { control })
    }

    /// Its control block: the thread pointer.
    #[cfg(test)]
    pub(crate) fn control(&self) -> *mut Control {
        self.control
    }

    /// Makes it the calling thread's: its FS base, and the thread's to free (`release_current`).
    pub(crate) fn install(self) {
        os::install(self.control.cast());
        core::mem::forget(self);
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: a block not installed is freed here, once, as it was allocated.
        unsafe { free(self.control) };
    }
}

/// Frees the allocation `control` was made in.
///
/// # Safety
///
/// `control` is a control block `Block::new` made, and nothing uses it or its TLS after.
unsafe fn free(control: *mut Control) {
    // SAFETY: the caller's contract: its fields are those `Block::new` wrote.
    unsafe {
        let (base, size, align) = ((*control).base, (*control).size, (*control).align);
        dealloc(base, Layout::from_size_align_unchecked(size, align));
    }
}

/// The calling thread's control block.
pub fn current() -> *mut Control {
    os::thread_pointer().cast()
}

/// Gives the first thread its block, before `main`. A program's first thread never ends but
/// with its process, so its block is never freed. `false` when there was no memory for it.
pub(crate) fn install_main() -> bool {
    match Block::new(&os::template()) {
        Some(block) => {
            block.install();
            true
        }
        None => false,
    }
}

/// At the calling thread's end, after its runtime's thread-local destructors: leaves it with no
/// thread pointer and frees its block.
pub(crate) fn release_current() {
    let control = current();
    os::install(ptr::null_mut());
    if !control.is_null() {
        // SAFETY: the thread's own block, which `install` gave it; its runtime's code has
        // ended, and nothing reads its thread-locals again.
        unsafe { free(control) };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    /// An ELF header naming `phdrs` program headers of `(type, vaddr, filesz, memsz, align)`.
    fn headers(phdrs: &[(u32, u64, u64, u64, u64)]) -> Vec<u8> {
        let mut bytes = std::vec![0u8; 64 + 56 * phdrs.len()];
        bytes[..4].copy_from_slice(&ELF_MAGIC);
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
        for (i, (kind, vaddr, filesz, memsz, align)) in phdrs.iter().enumerate() {
            let at = 64 + 56 * i;
            bytes[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            bytes[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
            bytes[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
            bytes[at + 48..at + 56].copy_from_slice(&align.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn the_template_is_the_pt_tls_header() {
        let bytes = headers(&[
            (1, 0x40_0000, 0x1000, 0x1000, 0x1000),
            (7, 0x86_f000, 24, 200, 16),
        ]);
        assert_eq!(
            Template::from_headers(&bytes),
            Some(Template {
                image: 0x86_f000,
                image_len: 24,
                size: 200,
                align: 16
            })
        );
        // No PT_TLS, an alignment of zero, and headers that are not an ELF image or not whole.
        assert_eq!(Template::from_headers(&headers(&[(1, 0, 0, 0, 0)])), None);
        let zero = Template::from_headers(&headers(&[(7, 1, 0, 8, 0)])).unwrap();
        assert_eq!(zero.align, 1);
        assert_eq!(Template::from_headers(b"\x7fELF"), None);
        assert_eq!(Template::from_headers(&[0u8; 64]), None);
        let mut cut = headers(&[(7, 1, 2, 3, 4)]);
        cut.truncate(100);
        assert_eq!(Template::from_headers(&cut), None);
    }

    #[test]
    fn the_thread_pointer_is_aligned_and_the_tls_ends_at_it() {
        for template in [
            Template {
                image: 0,
                image_len: 0,
                size: 200,
                align: 16,
            },
            Template {
                image: 0,
                image_len: 0,
                size: 64,
                align: 128,
            },
            Template::NONE,
        ] {
            let (layout, tp) = template.block().unwrap();
            assert_eq!(tp % template.align, 0);
            assert_eq!(tp % 64, 0);
            assert!(tp >= template.size, "the TLS fits below the pointer");
            assert_eq!(layout.size(), tp + size_of::<Control>());
            assert_eq!(layout.align(), template.align.max(64));
        }
        let huge = Template {
            image: 0,
            image_len: 0,
            size: usize::MAX,
            align: 64,
        };
        assert_eq!(huge.block(), None);
    }

    #[test]
    fn a_block_holds_the_image_then_zeros_and_its_control_block_points_at_itself() {
        static IMAGE: [u8; 5] = [1, 2, 3, 4, 5];
        let template = Template {
            image: IMAGE.as_ptr() as usize,
            image_len: IMAGE.len(),
            size: 40,
            align: 8,
        };
        let block = Block::new(&template).unwrap();
        let control = block.control();
        // SAFETY: the block's own memory, alive until it is dropped below.
        unsafe {
            let c = &*control;
            assert_eq!(c.tls_end, control.cast());
            assert_eq!(c.tcb_ptr, control);
            assert_eq!(c.tcb_len, size_of::<Control>());
            assert_eq!(c.tls_len, 40);
            assert!(c.libc.iter().all(|b| *b == 0));
            // A thread-local at offset `o` of the template is at `tp - tls_len + o`.
            let tls = core::slice::from_raw_parts(control.cast::<u8>().sub(40), 40);
            assert_eq!(&tls[..5], &IMAGE);
            assert!(tls[5..].iter().all(|b| *b == 0));
            assert_eq!((control as usize) % 64, 0);
        }
        drop(block);
    }

    #[test]
    fn a_thread_installs_its_block_and_releases_it_at_its_end() {
        std::thread::spawn(|| {
            assert!(current().is_null(), "a thread starts with no block");
            let block = Block::new(&os::template()).unwrap();
            let control = block.control();
            block.install();
            assert_eq!(current(), control);
            release_current();
            assert!(current().is_null());
            // The first thread's block, made from the program's template, stays.
            assert!(install_main());
            assert!(!current().is_null());
            release_current();
        })
        .join()
        .unwrap();
    }
}
