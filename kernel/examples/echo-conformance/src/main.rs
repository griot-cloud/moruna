//! SV-T11: runs `moruna.example.echo`'s conformance suite against the server the boot
//! description links this process to, and prints each failed check.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_svc::{Channel, Console, conformance, println, start};

moruna_svc::main!(run);

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(server) = start::take("server").map(Channel) else {
        println!(
            out,
            "KB2 conformance: FAIL: the boot description gave no server"
        );
        return 1;
    };
    let suite = echo_protocol::suite();
    let summary = conformance::run(&suite, &mut conformance::Listener(server), &mut |check| {
        if let Err(why) = check.result {
            println!(out, "KB2 conformance: FAIL: {}: {}", check.name, why);
        }
    });
    if summary.ok() {
        println!(
            out,
            "KB2 conformance {}: ok: {} checks passed against echo-server",
            suite.id.name,
            summary.passed
        );
        0
    } else {
        println!(
            out,
            "KB2 conformance {}: {} of {} checks failed",
            suite.id.name,
            summary.failed,
            summary.passed + summary.failed
        );
        1
    }
}
