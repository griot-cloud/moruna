//! The Arrow IPC check (contracts CT-I13, e.8) in `no_std` safe Rust, and the data plane's
//! record check (SDD 29).
//!
//! Every Arrow IPC byte Moruna reads is untrusted, and arrow-ipc panics, where it could return
//! an error, on some messages it cannot read. This crate is the one check: moruna-kernel's
//! `ipc::check` runs it before arrow-ipc sees a message (with the compressed batch's inflation
//! as the walk's [`walk::Plan`]), and the engine on the Moruna kernel runs [`record::check_record`]
//! on every record a task hands back before anything else sees it.
//!
//! - [`fb`]: a bounds-checked reader of the flatbuffers Arrow's messages are;
//! - [`format`]: the slots and enum values of `Message.fbs` and `Schema.fbs` it reads;
//! - [`schema`]: a schema message checked, and the model of its fields;
//! - [`walk`]: a record batch walked against that model, as arrow's decoder walks it;
//! - [`frame`]: the page-aligned record's framing (contracts e.7), read and rewritten in place;
//! - [`record`]: the data plane's record, checked, and turned into and out of a stream.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod fb;
pub mod format;
pub mod frame;
pub mod record;
pub mod schema;
pub mod walk;

#[cfg(test)]
mod tests;
