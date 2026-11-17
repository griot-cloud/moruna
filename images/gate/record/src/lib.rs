//! A gate function on the data plane (`architecture/data-os.md` section 5): the input batch as
//! the page-aligned Arrow record the engine mapped into the task, read in place, and the output
//! written as a record into the task's window.
//!
//! [`apply`] is the whole of what a native task does with a batch: it is the function a task
//! hands `moruna_sys::plane` (stream P's `Rings::step` takes exactly this shape, an input
//! record and a window to write the output record into, returning its length). It is `no_std`,
//! safe Rust, and host-tested against the oracle's fixtures (`tests/`).
//!
//! The record is read through the engine's own check (`moruna-ipc-check`); its buffers are
//! borrowed, never copied. The output is computed into the function's own vectors and copied
//! once into the window, laid out as `moruna_ipc_check::frame::page_layout` lays a record out.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod decode;
pub mod encode;
pub mod fb;
pub mod large;
pub mod nyc;
pub mod parts;
pub mod types;

use alloc::format;
use alloc::string::String;
use core::fmt;

/// Why a batch did not become a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The function refused the batch (`moruna_gate_functions::Error`).
    Function(moruna_gate_functions::Error),
    /// The record check refused the input, or the writer its own framing.
    Record(String),
    /// The output record is larger than the window the engine gave.
    WindowTooSmall { need: u64, have: u64 },
}

impl Error {
    pub fn layout(m: &str) -> Error {
        Error::Function(moruna_gate_functions::Error::Layout(String::from(m)))
    }

    pub fn schema(m: &str) -> Error {
        Error::Function(moruna_gate_functions::Error::Schema(String::from(m)))
    }
}

impl From<moruna_gate_functions::Error> for Error {
    fn from(e: moruna_gate_functions::Error) -> Error {
        Error::Function(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Function(e) => write!(f, "{e}"),
            Error::Record(m) => write!(f, "record: {m}"),
            Error::WindowTooSmall { need, have } => write!(
                f,
                "the output record takes {need} bytes and the window has {have}"
            ),
        }
    }
}

/// A gate job, as a native task runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    Enrich,
    Audit,
    Occupancy,
    Score,
    FwMinhash,
    OvShape,
    OvAttrs,
}

impl Job {
    pub fn by_name(name: &str) -> Option<Job> {
        Some(match name {
            "enrich" => Job::Enrich,
            "audit" => Job::Audit,
            "occupancy" => Job::Occupancy,
            "score" => Job::Score,
            "fw_minhash" => Job::FwMinhash,
            "ov_shape" => Job::OvShape,
            "ov_attrs" => Job::OvAttrs,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Job::Enrich => "enrich",
            Job::Audit => "audit",
            Job::Occupancy => "occupancy",
            Job::Score => "score",
            Job::FwMinhash => "fw_minhash",
            Job::OvShape => "ov_shape",
            Job::OvAttrs => "ov_attrs",
        }
    }
}

/// Runs `job` on the record `input` and writes its output record into `window`: the record's
/// length. An input the function refuses, or an output larger than the window, is an error and
/// leaves no record.
pub fn apply(job: Job, input: &[u8], window: &mut [u8]) -> Result<usize, Error> {
    let batch = decode::Batch::read(input)?;
    let out = compute(job, &batch)?;
    encode::write(&out.fields, &out.columns, out.rows, window)
}

/// `job`'s output for a record read.
pub fn compute<'a>(job: Job, batch: &decode::Batch<'a>) -> Result<Output<'a>, Error> {
    match job {
        Job::Enrich => nyc::enrich(batch),
        Job::Audit => nyc::audit(batch),
        Job::Occupancy => nyc::occupancy(batch),
        Job::Score => nyc::score(batch),
        Job::FwMinhash => large::fw_minhash(batch),
        Job::OvShape => large::ov_shape(batch),
        Job::OvAttrs => large::ov_attrs(batch),
    }
}

/// A function's output: its fields and columns.
pub struct Output<'a> {
    pub rows: usize,
    pub fields: alloc::vec::Vec<types::LField>,
    pub columns: alloc::vec::Vec<encode::OutArray<'a>>,
}

impl<'a> Output<'a> {
    pub fn new(rows: usize) -> Output<'a> {
        Output {
            rows,
            fields: alloc::vec::Vec::new(),
            columns: alloc::vec::Vec::new(),
        }
    }

    pub fn push(&mut self, field: types::LField, column: encode::OutArray<'a>) {
        self.fields.push(field);
        self.columns.push(column);
    }
}

/// For messages.
pub fn describe(e: &Error) -> String {
    format!("{e}")
}
