//! The output layer on `disk.out` (contract C3, `crate::devices`): at the run's end the
//! sinks' committed files are one layer file in `moruna-image`'s format, written onto the
//! output disk from its first byte with one flush, and nothing else is ever written there.
//! The host reads it back with `moruna-layer` (stream D).
//!
//! The file is moved in pieces through two arena buffers, so one piece is on the device's ring
//! while the next is read: the device reads each piece by DMA from the arena.

use std::io::Read;
use std::path::Path;
use std::sync::mpsc;

use moruna_kernel::{Allocator, Tier};

use super::disk::Disk;
use super::engine::{Kind, Op};
use super::plan::SECTOR;

/// The bytes of one piece.
pub const PIECE: usize = 1 << 20;

/// What writing the layer did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// The file's bytes.
    pub bytes: u64,
    /// The bytes written onto the disk: the file, its last sector filled with zeros.
    pub on_disk: u64,
    /// Pieces written.
    pub pieces: u64,
}

/// Writes the file at `path` onto `disk` from its first byte, in pieces of at most `piece`
/// bytes (whole sectors) through buffers of `alloc` in `tier`, then flushes the disk's cache.
pub fn write_file(
    disk: &Disk,
    path: &Path,
    alloc: &dyn Allocator,
    tier: Tier,
    piece: usize,
) -> Result<Written, String> {
    let piece = piece / SECTOR as usize * SECTOR as usize;
    if piece == 0 {
        return Err("a piece is at least a sector".into());
    }
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("opening the layer {}: {e}", path.display()))?;
    let bytes = file
        .metadata()
        .map_err(|e| format!("the layer {}: {e}", path.display()))?
        .len();
    let on_disk = bytes.next_multiple_of(SECTOR);
    if on_disk > disk.capacity() {
        return Err(format!(
            "the layer's {bytes} bytes do not fit {}'s {} bytes",
            disk.name(),
            disk.capacity()
        ));
    }
    let mut buffers = [
        alloc
            .alloc(piece, tier)
            .map_err(|e| format!("a buffer for the layer: {e}"))?,
        alloc
            .alloc(piece, tier)
            .map_err(|e| format!("a buffer for the layer: {e}"))?,
    ];
    let mut written = Written {
        bytes,
        on_disk,
        pieces: 0,
    };
    let mut pending: [Option<mpsc::Receiver<Result<(), String>>>; 2] = [None, None];
    let mut offset = 0u64;
    let mut which = 0;
    let mut failed = None;
    while offset < bytes && failed.is_none() {
        // This buffer's last write is done before the buffer is filled again.
        if let Some(rx) = pending[which].take()
            && let Err(why) = wait(rx)
        {
            failed = Some(why);
            break;
        }
        let buffer = &mut buffers[which];
        let want = ((bytes - offset) as usize).min(piece);
        if let Err(e) = file.read_exact(&mut buffer[..want]) {
            failed = Some(format!("reading the layer {}: {e}", path.display()));
            break;
        }
        let len = want.next_multiple_of(SECTOR as usize);
        buffer[want..len].fill(0);
        let Some(addr) = buffer.host_ptr() else {
            failed = Some("a layer buffer that is not in host memory".into());
            break;
        };
        let (tx, rx) = mpsc::channel();
        disk.submit(Op {
            kind: Kind::Write,
            offset,
            addr: addr as u64,
            len: len as u64,
            done: Box::new(move |r| {
                let _ = tx.send(r);
            }),
        });
        pending[which] = Some(rx);
        written.pieces += 1;
        offset += want as u64;
        which ^= 1;
    }
    // The buffers outlive every write the device may still be reading them for.
    for rx in pending.iter_mut().filter_map(Option::take) {
        if let Err(why) = wait(rx) {
            failed.get_or_insert(why);
        }
    }
    if let Some(why) = failed {
        return Err(why);
    }
    disk.run(Op {
        kind: Kind::Flush,
        offset: 0,
        addr: 0,
        len: 0,
        done: Box::new(|_| {}),
    })?;
    Ok(written)
}

fn wait(rx: mpsc::Receiver<Result<(), String>>) -> Result<(), String> {
    rx.recv()
        .unwrap_or_else(|_| Err("the disk dropped a write".into()))
}
