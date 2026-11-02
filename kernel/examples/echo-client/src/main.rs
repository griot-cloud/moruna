//! The echo client (SV-T10): connects to the echo server through the listener the
//! boot description links it to, and checks the exchange through the typed client.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec};

use echo_protocol::echo;
use moruna_svc::{CallError, Channel, Console, connect, deadline_in, println, start};

moruna_svc::main!(run);

const SECOND: u64 = 1_000_000_000;

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(server) = start::take("server").map(Channel) else {
        println!(out, "KB2 echo: FAIL: the boot description gave no server");
        return 1;
    };
    match exchange(&server) {
        Ok(()) => {
            println!(
                out,
                "KB2 echo: ok: echoed and reversed text, got the echoed event, was refused an empty text, sent a channel and read what the server wrote to it, echoed 60000 bytes, and was refused 70000 bytes before sending"
            );
            0
        }
        Err(why) => {
            println!(out, "KB2 echo: FAIL: {}", why);
            1
        }
    }
}

fn exchange(server: &Channel) -> Result<(), String> {
    let channel = connect(server).map_err(|e| format!("connect: {e:?}"))?;
    let mut c = echo::Client::connect(channel, deadline_in(10 * SECOND))
        .map_err(|e| format!("handshake: {e:?}"))?;
    c.core().set_timeout(10 * SECOND);
    if c.core().server_minor() != echo::ID.minor {
        return Err(format!("the server speaks 1.{}", c.core().server_minor()));
    }
    let text = b"hello, kernel".to_vec();
    if c.echo(text.clone()).map_err(|e| format!("echo: {e:?}"))? != text {
        return Err(String::from("echo changed the text"));
    }
    match c.next_event(deadline_in(SECOND)) {
        Ok(echo::Event::echoed { total: 13 }) => {}
        other => return Err(format!("the echoed event: {other:?}")),
    }
    if c.reverse(b"abc".to_vec())
        .map_err(|e| format!("reverse: {e:?}"))?
        != b"cba"
    {
        return Err(String::from("reverse did not reverse"));
    }
    match c.echo(vec![]) {
        Err(CallError::Remote(echo::Error::Empty)) => {}
        other => return Err(format!("an empty echo answered {other:?}")),
    }
    let (mine, theirs) = Channel::create().map_err(|e| format!("channel_create: {}", e.name()))?;
    if c.send_to(theirs, b"over a handle".to_vec())
        .map_err(|e| format!("send_to: {e:?}"))?
        != 13
    {
        return Err(String::from("send_to reported the wrong length"));
    }
    let mut buf = [0u8; 32];
    match mine.read(&mut buf, &mut []) {
        Ok((13, 0)) if &buf[..13] == b"over a handle" => {}
        other => return Err(format!("the channel carried {other:?}")),
    }
    let large = vec![0x5au8; 60_000];
    if c.echo(large.clone())
        .map_err(|e| format!("a large echo: {e:?}"))?
        != large
    {
        return Err(String::from("a large echo came back changed"));
    }
    match c.echo(vec![0u8; 70_000]) {
        Err(CallError::TooLarge) => {}
        other => return Err(format!("an echo over the bound answered {other:?}")),
    }
    Ok(())
}
