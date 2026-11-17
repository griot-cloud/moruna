//! Host tests of `moruna-vmbus`: the client against a fake host that is strict about
//! order and sizes (`host`), and the in-memory channel against a fake device.

mod channel;
mod contact;
mod fuzz;
mod garbage;
mod gpadl;
mod host;
mod open;
mod rescind;
mod strict;
