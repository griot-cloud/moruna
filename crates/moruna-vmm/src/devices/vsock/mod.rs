//! `virtio-vsock`: the guest's only channel to the world (MH 4.8.2).
//!
//! Queue 0 carries packets to the guest, queue 1 packets from it, queue 2 events (the device
//! sends none: there is no migration to announce). The transmit queue is served in the vCPU
//! thread that notified it; host sockets are served by the backend thread through
//! [`VirtioVsock::backend_ready`]. Both hold the transport's lock, so the muxer is only ever
//! touched by one thread at a time.

pub mod muxer;
pub mod packet;

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

use virtio_queue::{Queue, QueueT};

use self::muxer::{Connector, MAX_PKT_PAYLOAD, Muxer, Token, Waker};
use self::packet::{HDR_BYTES, Header};
use super::Mem;
use super::virtio::{ID_VSOCK, Interrupt, QUEUE_MAX_SIZE, VirtioDevice, read_config_bytes};
use crate::error::{Result, VmmError};
use crate::sys::{PollFd, Ready};

/// Receive queue index.
pub const RXQ: usize = 0;
/// Transmit queue index.
pub const TXQ: usize = 1;
/// Event queue index.
pub const EVQ: usize = 2;

/// The device.
pub struct VirtioVsock {
    cid: u32,
    muxer: Muxer,
    waker: Waker,
}

impl VirtioVsock {
    /// The vsock device of guest `cid`, its host side at `uds_path`.
    pub fn new(cid: u32, uds_path: &Path) -> Result<Self> {
        let muxer = Muxer::new(cid, uds_path)?;
        let waker = muxer.waker()?;
        Ok(VirtioVsock { cid, muxer, waker })
    }

    /// The guest's context id.
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// The muxer (for the backend thread's poll set and for tests).
    pub fn muxer(&self) -> &Muxer {
        &self.muxer
    }

    /// A handle that wakes the backend thread.
    pub fn waker(&self) -> Result<Waker> {
        self.muxer.waker()
    }

    /// Serve guest connections to host port `port` in-process.
    pub fn add_connector(&mut self, port: u32, connector: Connector) {
        self.muxer.add_connector(port, connector);
    }

    /// Open a connection from the monitor to guest port `port` over `stream`.
    pub fn connect_to_guest(&mut self, stream: UnixStream, port: u32) -> Result<()> {
        self.muxer.connect_to_guest(stream, port)?;
        self.waker.wake();
        Ok(())
    }

    /// What the backend thread polls.
    pub fn poll_set(&self) -> Vec<(PollFd, Token)> {
        self.muxer.poll_set()
    }

    /// The backend thread's poll returned; serve the host side and fill the receive queue.
    /// Returns true when used buffers were returned.
    pub fn backend_ready(
        &mut self,
        ready: &[(Token, Ready)],
        queues: &mut [Queue],
        mem: &Mem,
    ) -> Result<bool> {
        if !self.muxer.on_ready(ready) {
            return Ok(false);
        }
        self.fill_rx(queues, mem)
    }

    fn fill_rx(&mut self, queues: &mut [Queue], mem: &Mem) -> Result<bool> {
        match queues.get_mut(RXQ) {
            Some(q) if q.ready() => self.muxer.fill_rx(q, mem),
            _ => Ok(false),
        }
    }

    fn process_tx(&mut self, q: &mut Queue, mem: &Mem) -> Result<bool> {
        let mut any = false;
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            if let Ok(mut r) = chain.reader(mem) {
                let mut raw = [0u8; HDR_BYTES];
                if r.read_exact(&mut raw).is_ok() {
                    let h = Header::decode(&raw);
                    let len = h.len as usize;
                    // A packet claiming more payload than it carries, or more than a packet
                    // may, is dropped.
                    if len <= MAX_PKT_PAYLOAD && len <= r.available_bytes() {
                        let mut payload = vec![0u8; len];
                        if r.read_exact(&mut payload).is_ok() {
                            self.muxer.recv(&h, &payload);
                        }
                    }
                }
            }
            q.add_used(mem, head, 0)
                .map_err(|e| VmmError::device("virtio-vsock", format!("used ring: {e}")))?;
            any = true;
        }
        Ok(any)
    }
}

impl VirtioDevice for VirtioVsock {
    fn name(&self) -> &'static str {
        "virtio-vsock"
    }

    fn device_type(&self) -> u32 {
        ID_VSOCK
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![QUEUE_MAX_SIZE; 3]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&(self.cid as u64).to_le_bytes(), offset, data);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, _acked: u64, _interrupt: Arc<Interrupt>) -> Result<()> {
        self.waker.wake();
        Ok(())
    }

    fn process_queue(&mut self, index: usize, queues: &mut [Queue], mem: &Mem) -> Result<bool> {
        let used = match index {
            TXQ => {
                let q = queues
                    .get_mut(TXQ)
                    .ok_or_else(|| VmmError::device("virtio-vsock", "no tx queue"))?;
                let tx = self.process_tx(q, mem)?;
                self.muxer.flush_all();
                tx | self.fill_rx(queues, mem)?
            }
            RXQ => self.fill_rx(queues, mem)?,
            _ => false,
        };
        // What to poll may have changed: new connections, buffers, credit.
        self.waker.wake();
        Ok(used)
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests;
