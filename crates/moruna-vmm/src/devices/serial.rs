//! The 8250 serial console: `vm-superio`'s UART, its output copied to a sink (the monitor's
//! stdout) and into a bounded log (MH 4.8.2, "console to the monitor's stdout").
//!
//! The log keeps the last [`LOG_BYTES`] bytes so that, when the guest kernel panics, the
//! monitor can write the console's tail to stderr and exit 6 (MH 6). A panic is recognised by
//! the line the kernel prints before anything else, [`PANIC_MARKER`]. There is no console
//! input: the guest's only inbound channel is vsock.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vm_superio::{Serial, Trigger};

use super::{Device, Irq};

/// Bytes of console output kept for the panic report: 64 KiB holds a full oops with its
/// backtrace and the lines before it.
pub const LOG_BYTES: usize = 64 << 10;
/// The line prefix Linux prints when it panics.
pub const PANIC_MARKER: &str = "Kernel panic - not syncing";
/// A console line longer than this is split for panic detection (a panic line is short).
pub const LINE_MAX: usize = 4096;
/// The x86_64 COM1 port base.
pub const COM1_PORT: u64 = 0x3f8;
/// The COM1 IRQ line.
pub const COM1_IRQ: u32 = 4;
/// The 8250's register window: eight byte-wide registers.
pub const WINDOW_BYTES: u64 = 8;

/// The console's memory: a bounded tail and whether a panic line went past.
#[derive(Default)]
pub struct ConsoleLog {
    tail: Mutex<VecDeque<u8>>,
    line: Mutex<Vec<u8>>,
    panicked: AtomicBool,
}

impl ConsoleLog {
    /// A new, empty log.
    pub fn new() -> Arc<Self> {
        Arc::new(ConsoleLog::default())
    }

    /// Record bytes the guest wrote.
    pub fn record(&self, bytes: &[u8]) {
        {
            let mut tail = self.tail.lock().unwrap_or_else(|p| p.into_inner());
            tail.extend(bytes);
            let excess = tail.len().saturating_sub(LOG_BYTES);
            tail.drain(..excess);
        }
        let mut line = self.line.lock().unwrap_or_else(|p| p.into_inner());
        for &b in bytes {
            if b == b'\n' || line.len() >= LINE_MAX {
                self.check(&line);
                line.clear();
                if b == b'\n' {
                    continue;
                }
            }
            line.push(b);
        }
        // A panic line is recognised before its newline arrives: the guest may die mid-line.
        self.check(&line);
    }

    fn check(&self, line: &[u8]) {
        if !self.panicked.load(Ordering::Relaxed)
            && line
                .windows(PANIC_MARKER.len())
                .any(|w| w == PANIC_MARKER.as_bytes())
        {
            self.panicked.store(true, Ordering::SeqCst);
        }
    }

    /// True once the guest printed a kernel panic.
    pub fn panicked(&self) -> bool {
        self.panicked.load(Ordering::SeqCst)
    }

    /// The last [`LOG_BYTES`] bytes of console output.
    pub fn tail(&self) -> Vec<u8> {
        self.tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect()
    }
}

/// Where console bytes go: a sink (stdout in the monitor) and the log.
pub struct ConsoleOut {
    sink: Box<dyn Write + Send>,
    log: Arc<ConsoleLog>,
}

impl ConsoleOut {
    /// Bytes go to `sink` and into `log`.
    pub fn new(sink: Box<dyn Write + Send>, log: Arc<ConsoleLog>) -> Self {
        ConsoleOut { sink, log }
    }
}

impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.log.record(buf);
        // The console is best effort: a closed stdout must not stop the guest.
        let _ = self.sink.write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.sink.flush();
        Ok(())
    }
}

/// `vm-superio`'s trigger over the monitor's interrupt line.
pub struct IrqTrigger(pub Arc<dyn Irq>);

impl Trigger for IrqTrigger {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        self.0.trigger()
    }
}

/// The console device.
pub struct SerialConsole {
    uart: Serial<IrqTrigger, vm_superio::serial::NoEvents, ConsoleOut>,
}

impl SerialConsole {
    /// A console raising `irq`, writing to `out`.
    pub fn new(irq: Arc<dyn Irq>, out: ConsoleOut) -> Self {
        SerialConsole {
            uart: Serial::new(IrqTrigger(irq), out),
        }
    }
}

impl Device for SerialConsole {
    fn name(&self) -> &'static str {
        "serial"
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match (data.len(), u8::try_from(offset)) {
            (1, Ok(o)) if (o as u64) < WINDOW_BYTES => data[0] = self.uart.read(o),
            _ => data.fill(0),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if let (1, Ok(o)) = (data.len(), u8::try_from(offset))
            && (o as u64) < WINDOW_BYTES
        {
            // A failed interrupt trigger loses at most an interrupt the guest does not
            // wait for (the 8250 driver polls LSR); the byte itself was written.
            let _ = self.uart.write(o, data[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::counting_irq;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn console() -> (SerialConsole, Shared, Arc<ConsoleLog>) {
        let (_, irq) = counting_irq();
        let out = Shared::default();
        let log = ConsoleLog::new();
        (
            SerialConsole::new(irq, ConsoleOut::new(Box::new(out.clone()), log.clone())),
            out,
            log,
        )
    }

    #[test]
    fn vm_t15_console_bytes_reach_stdout_and_the_log() {
        let (mut s, out, log) = console();
        for b in b"hello\n" {
            s.write(0, &[*b]);
        }
        assert_eq!(&*out.0.lock().unwrap(), b"hello\n");
        assert_eq!(log.tail(), b"hello\n");
        assert!(!log.panicked());
        // LSR says the transmitter is empty, so the Linux driver keeps writing.
        let mut lsr = [0u8];
        s.read(5, &mut lsr);
        assert_ne!(lsr[0] & 0x20, 0);
        // Wide or out-of-window accesses are ignored.
        let mut two = [1u8, 1];
        s.read(0, &mut two);
        assert_eq!(two, [0, 0]);
        s.write(0, &[1, 2]);
        s.write(9, b"x");
        s.read(300, &mut lsr);
        assert_eq!(lsr[0], 0);
        assert_eq!(out.0.lock().unwrap().len(), 6);
        assert_eq!(s.name(), "serial");
        let mut c = ConsoleOut::new(Box::new(Shared::default()), log);
        c.flush().unwrap();
    }

    #[test]
    fn vm_t15_panic_is_detected_and_the_log_is_bounded() {
        let log = ConsoleLog::new();
        log.record(&vec![b'a'; LOG_BYTES + 100]);
        assert_eq!(log.tail().len(), LOG_BYTES);
        assert!(!log.panicked());
        // Split across writes and not yet terminated.
        log.record(b"\n[ 1.0] Kernel pa");
        assert!(!log.panicked());
        log.record(b"nic - not syncing: Attempted to kill init");
        assert!(log.panicked());
        assert!(String::from_utf8_lossy(&log.tail()).ends_with("kill init"));
        // Lines are bounded; ordinary output is not a panic.
        let log = ConsoleLog::new();
        log.record(&vec![b'x'; LINE_MAX * 2 + 3]);
        log.record(b"\nok\n");
        assert!(!log.panicked());
    }
}
