//! The echo server (SV-T10, SV-T11): serves `moruna.example.echo` on every listener
//! the boot description gives it (each start slot whose name begins with `listen`),
//! one connection per client, until every client and listener has gone.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{rc::Rc, vec::Vec};
use core::cell::Cell;

use echo_protocol::echo;
use moruna_svc::{Channel, Console, Context, Dispatcher, println, start};

moruna_svc::main!(run);

struct Echo {
    total: u64,
}

impl echo::Server for Echo {
    fn echo(&mut self, cx: &mut Context<'_>, text: Vec<u8>) -> Result<Vec<u8>, echo::Error> {
        if text.is_empty() {
            return Err(echo::Error::Empty);
        }
        self.total += text.len() as u64;
        // A client too old for an event, or one whose queue is full, still gets its
        // reply; the dispatcher closes the latter after it.
        let _ = echo::Event::echoed { total: self.total }.emit(cx);
        Ok(text)
    }

    fn reverse(&mut self, _: &mut Context<'_>, mut text: Vec<u8>) -> Result<Vec<u8>, echo::Error> {
        if text.is_empty() {
            return Err(echo::Error::Empty);
        }
        text.reverse();
        Ok(text)
    }

    fn send_to(
        &mut self,
        _: &mut Context<'_>,
        to: Channel,
        text: Vec<u8>,
    ) -> Result<u32, echo::Error> {
        to.write_with(&text, &mut [])
            .map_err(|_| echo::Error::PeerGone)?;
        Ok(text.len() as u32)
    }
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let served = Rc::new(Cell::new(0u32));
    let mut dispatcher = Dispatcher::new();
    let mut listeners = 0;
    for i in 0..start::count() {
        let Some((_, _, slot)) = start::entry(i) else {
            continue;
        };
        if !slot.starts_with("listen") {
            continue;
        }
        let Some(handle) = start::take(slot) else {
            continue;
        };
        let count = served.clone();
        let factory = echo::factory(move || {
            count.set(count.get() + 1);
            Echo { total: 0 }
        });
        if let Err(e) = dispatcher.listen(Channel(handle), factory) {
            println!(
                out,
                "KB2 echo-server: FAIL: listening on {}: {}",
                slot,
                e.name()
            );
            return 1;
        }
        listeners += 1;
    }
    if listeners == 0 {
        println!(
            out,
            "KB2 echo-server: FAIL: the boot description gave no listener"
        );
        return 1;
    }
    match dispatcher.run() {
        Ok(()) => {
            println!(
                out,
                "KB2 echo-server: ok: served {} connections on {} listeners until every client left",
                served.get(),
                listeners
            );
            0
        }
        Err(e) => {
            println!(
                out,
                "KB2 echo-server: FAIL: the dispatch loop: {}",
                e.name()
            );
            1
        }
    }
}
