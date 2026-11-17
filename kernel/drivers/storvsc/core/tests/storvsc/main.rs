//! Host tests of `moruna-storvsc` against a fake storvsp (item 1.10a).

mod fake;

mod errors;
mod fuzz;
mod init;
mod io;

use std::{cell::RefCell, rc::Rc};

use fake::{DISK, Host, Link, pair};
use moruna_storvsc::{Completion, Disk, Event, PAGE, Storvsc, Wait};

/// Polls until the initialisation finishes.
fn ready(device: &mut Storvsc<Link>) {
    loop {
        match device.poll().expect("the initialisation") {
            Some(Event::Ready) => return,
            Some(event) => panic!("{event:?} before the device was ready"),
            None => assert_eq!(
                device.wait(None).expect("the host's next step"),
                Wait::Signalled
            ),
        }
    }
}

/// A device that has finished its initialisation against the fake with its defaults.
fn device() -> (Storvsc<Link>, Rc<RefCell<Host>>) {
    let (link, host) = pair();
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    (device, host)
}

/// The next completion, which must be there.
fn next(device: &mut Storvsc<Link>) -> Completion {
    match device.poll().expect("a completion") {
        Some(Event::Completed(completion)) => completion,
        other => panic!("{other:?} where a completion was due"),
    }
}

/// The completion of `id`, which must be the next one, and the bytes it moved.
fn moved(device: &mut Storvsc<Link>, id: u64) -> u32 {
    let completion = next(device);
    assert_eq!(completion.id, id);
    completion.result.expect("the request")
}

/// Reads the capacity of the disk every test starts with, through guest page `page`.
fn disk(device: &mut Storvsc<Link>, host: &Rc<RefCell<Host>>, page: u64) -> Disk {
    let id = device.read_capacity(DISK, page).unwrap();
    let moved = moved(device, id) as usize;
    let answer = host.borrow().memory.read(&[page], moved).unwrap();
    Disk::from_capacity(DISK, &answer).unwrap()
}

/// `n` bytes that differ from block to block and from seed to seed.
fn pattern(seed: u64, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| {
            (i as u64)
                .wrapping_mul(31)
                .wrapping_add(seed.wrapping_mul(131)) as u8
                ^ 0x5a
        })
        .collect()
}

/// The pages a transfer of `bytes` needs.
fn pages_for(bytes: usize) -> usize {
    bytes.div_ceil(PAGE)
}
