// SPDX-License-Identifier: MPL-2.0

//! DMA buffers, in user space: plain memory.
//!
//! The personality drives no device; its block and DMA types only carry buffers between
//! its own parts. A DMA buffer is a segment of the personality's memory, its "device
//! address" is its address, and syncs are no-ops.

use core::{fmt::Debug, marker::PhantomData, ops::Range};

use super::{
    Daddr, FrameAllocOptions, HasDaddr, HasPaddr, HasSize, Infallible, Paddr, Split, USegment,
    VmReader, VmWriter,
    io::util::{HasVmReaderWriter, VmReaderWriterIdentity, VmReaderWriterResult},
};
use crate::{Error, Result};

mod private {
    pub trait Sealed {}
}

/// Which way data flows between the CPU and a device.
pub trait DmaDirection: 'static + Debug + private::Sealed {
    /// Whether the CPU reads what the device wrote.
    const CAN_READ_FROM_DEVICE: bool;
    /// Whether the CPU writes for the device.
    const CAN_WRITE_TO_DEVICE: bool;
}

/// To the device.
#[derive(Debug)]
pub enum ToDevice {}
impl private::Sealed for ToDevice {}
impl DmaDirection for ToDevice {
    const CAN_READ_FROM_DEVICE: bool = false;
    const CAN_WRITE_TO_DEVICE: bool = true;
}

/// From the device.
#[derive(Debug)]
pub enum FromDevice {}
impl private::Sealed for FromDevice {}
impl DmaDirection for FromDevice {
    const CAN_READ_FROM_DEVICE: bool = true;
    const CAN_WRITE_TO_DEVICE: bool = false;
}

/// Both ways.
#[derive(Debug)]
pub enum FromAndToDevice {}
impl private::Sealed for FromAndToDevice {}
impl DmaDirection for FromAndToDevice {
    const CAN_READ_FROM_DEVICE: bool = true;
    const CAN_WRITE_TO_DEVICE: bool = true;
}

fn alloc(nframes: usize, zeroed: bool) -> Result<USegment> {
    Ok(FrameAllocOptions::new()
        .zeroed(zeroed)
        .alloc_segment(nframes)?
        .into())
}

/// A streaming DMA buffer.
#[derive(Debug)]
pub struct DmaStream<D: DmaDirection = FromAndToDevice> {
    segment: USegment,
    _direction: PhantomData<D>,
}

impl<D: DmaDirection> DmaStream<D> {
    /// A zeroed buffer of `nframes` pages.
    pub fn alloc(nframes: usize, _is_cache_coherent: bool) -> Result<Self> {
        Self::map(alloc(nframes, true)?, false)
    }

    /// A buffer of `nframes` pages, not zeroed.
    pub fn alloc_uninit(nframes: usize, _is_cache_coherent: bool) -> Result<Self> {
        Self::map(alloc(nframes, false)?, false)
    }

    /// A buffer over `segment`.
    pub fn map(segment: USegment, _is_cache_coherent: bool) -> Result<Self> {
        Ok(Self {
            segment,
            _direction: PhantomData,
        })
    }

    /// Makes what the device wrote in `byte_range` visible: nothing to do.
    pub fn sync_from_device(&self, byte_range: Range<usize>) -> Result<()> {
        self.check(byte_range, D::CAN_READ_FROM_DEVICE)
    }

    /// Makes what the CPU wrote in `byte_range` visible to the device: nothing to do.
    pub fn sync_to_device(&self, byte_range: Range<usize>) -> Result<()> {
        self.check(byte_range, D::CAN_WRITE_TO_DEVICE)
    }

    fn check(&self, byte_range: Range<usize>, allowed: bool) -> Result<()> {
        if !allowed {
            return Err(Error::AccessDenied);
        }
        if byte_range.end > self.size() {
            return Err(Error::InvalidArgs);
        }
        Ok(())
    }
}

impl<D: DmaDirection> Split for DmaStream<D> {
    fn split(self, offset: usize) -> (Self, Self) {
        let (a, b) = self.segment.split(offset);
        (
            Self {
                segment: a,
                _direction: PhantomData,
            },
            Self {
                segment: b,
                _direction: PhantomData,
            },
        )
    }
}

impl<D: DmaDirection> HasPaddr for DmaStream<D> {
    fn paddr(&self) -> Paddr {
        self.segment.paddr()
    }
}

impl<D: DmaDirection> HasDaddr for DmaStream<D> {
    fn daddr(&self) -> Daddr {
        self.segment.paddr()
    }
}

impl<D: DmaDirection> HasSize for DmaStream<D> {
    fn size(&self) -> usize {
        self.segment.size()
    }
}

impl<D: DmaDirection> HasVmReaderWriter for DmaStream<D> {
    type Types = VmReaderWriterResult;

    fn reader(&self) -> Result<VmReader<'_, Infallible>> {
        if !D::CAN_READ_FROM_DEVICE {
            return Err(Error::AccessDenied);
        }
        Ok(self.segment.reader())
    }

    fn writer(&self) -> Result<VmWriter<'_, Infallible>> {
        if !D::CAN_WRITE_TO_DEVICE {
            return Err(Error::AccessDenied);
        }
        Ok(self.segment.writer())
    }
}

/// A coherent DMA buffer.
#[derive(Debug)]
pub struct DmaCoherent {
    segment: USegment,
}

impl DmaCoherent {
    /// A zeroed buffer of `nframes` pages.
    pub fn alloc(nframes: usize, _is_cache_coherent: bool) -> Result<Self> {
        Ok(Self {
            segment: alloc(nframes, true)?,
        })
    }

    /// A buffer of `nframes` pages, not zeroed.
    pub fn alloc_uninit(nframes: usize, _is_cache_coherent: bool) -> Result<Self> {
        Ok(Self {
            segment: alloc(nframes, false)?,
        })
    }
}

impl Split for DmaCoherent {
    fn split(self, offset: usize) -> (Self, Self) {
        let (a, b) = self.segment.split(offset);
        (Self { segment: a }, Self { segment: b })
    }
}

impl HasPaddr for DmaCoherent {
    fn paddr(&self) -> Paddr {
        self.segment.paddr()
    }
}

impl HasDaddr for DmaCoherent {
    fn daddr(&self) -> Daddr {
        self.segment.paddr()
    }
}

impl HasSize for DmaCoherent {
    fn size(&self) -> usize {
        self.segment.size()
    }
}

impl HasVmReaderWriter for DmaCoherent {
    type Types = VmReaderWriterIdentity;

    fn reader(&self) -> VmReader<'_, Infallible> {
        self.segment.reader()
    }

    fn writer(&self) -> VmWriter<'_, Infallible> {
        self.segment.writer()
    }
}
