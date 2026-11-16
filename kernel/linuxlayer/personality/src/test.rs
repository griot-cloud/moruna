// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use component::InitStage;
use ostd::mm::Infallible;

use super::*;

#[test]
fn cmdline_stops_at_zero_and_trims() {
    assert_eq!(
        cmdline(b" init=/init -- a b \0\0\0garbage"),
        "init=/init -- a b"
    );
    assert_eq!(cmdline(b""), "");
    assert_eq!(cmdline(b"x\xffy"), "x\u{fffd}y");
}

#[test]
fn boot_info_gives_the_budget() {
    static RAMFS: [u8; 4] = *b"0707";
    let info = boot_info(String::from("init=/x"), Some(&RAMFS));
    assert_eq!(info.bootloader_name, "moruna");
    assert_eq!(info.kernel_cmdline, "init=/x");
    assert_eq!(info.initramfs, Some(&RAMFS[..]));
    assert_eq!(info.memory_regions.len(), 1);
    assert_eq!(info.memory_regions[0].len(), MEMORY_BUDGET);
    assert_eq!(info.wall_clock_at_start, Duration::ZERO);
}

static SENT: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());
static RECEIVED: AtomicUsize = AtomicUsize::new(0);

fn record(bytes: &[u8]) {
    SENT.lock().extend_from_slice(bytes);
}

fn count(reader: VmReader<Infallible>) {
    RECEIVED.fetch_add(reader.remain(), Ordering::SeqCst);
}

#[test]
fn console_sends_and_hands_input_on() {
    let console = Console::new(record);
    console.send(b"hello");
    assert_eq!(SENT.lock().as_slice(), b"hello");
    console.receive(b"ignored: no callback yet");
    assert_eq!(RECEIVED.load(Ordering::SeqCst), 0);
    console.register_callback(&count);
    console.register_callback(&count);
    console.receive(b"abc");
    assert_eq!(RECEIVED.load(Ordering::SeqCst), 6);
    assert!(alloc::format!("{console:?}").contains("callbacks: 2"));
}

#[test]
fn start_boots_in_a_task_with_the_console_registered() {
    let mut ran = false;
    start(boot_info(String::new(), None), || {
        assert!(Task::current().is_some());
        assert_eq!(ostd::boot::boot_info().bootloader_name, "moruna");
        component::init_all(InitStage::Bootstrap, component::parse_metadata!()).unwrap();
        let devices = aster_console::all_devices();
        let (_, console) = devices
            .iter()
            .find(|(name, _)| name == aster_console::UART_CONSOLE_NAME)
            .expect("the console is registered");
        console.send(b"personality console\n");
        ran = true;
    });
    assert!(ran);
    assert!(Task::current().is_none());
}
