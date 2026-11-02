//! `moruna.linux` 3.0 (SDD 28, d.3; SDD 30 d.2): runs an unmodified Linux program as a
//! process on the Moruna microkernel, from the image the personality server's image
//! filesystem serves, and answers when it has ended: how, with what code, what it wrote
//! to its standard output and error, and which calls it was refused. A program lives no
//! longer than the connection that asked for it: closing the connection ends it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_svc::{Channel, Decoder, Encoder, Handle, Wire, WireError};

/// The most bytes of a program's output an answer carries; the rest is counted.
pub const OUTPUT_MAX: usize = 16 * 1024;
/// The most bytes of path, arguments and environment a request carries.
pub const REQUEST_MAX: usize = 48 * 1024;

/// Bytes, carried as one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Wire for Bytes {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.0.len())?;
        e.put_raw(&self.0);
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        Ok(Bytes(d.take_raw(n)?.to_vec()))
    }
}

impl From<&[u8]> for Bytes {
    fn from(b: &[u8]) -> Bytes {
        Bytes(b.to_vec())
    }
}

/// How a program ended.
pub mod how {
    /// It called `exit_group`, or its last thread `exit`: `code` is its code.
    pub const EXITED: u8 = 1;
    /// A signal it did not handle ended it: `code` is the signal.
    pub const SIGNALED: u8 = 2;
    /// It faulted, or was killed: the kernel's console line says which.
    pub const FAULTED: u8 = 3;
}

/// What a program did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ended {
    pub how: u8,
    pub code: i64,
    /// Its standard output and error, interleaved as written, up to `OUTPUT_MAX`.
    pub output: Bytes,
    /// Output bytes past `OUTPUT_MAX`.
    pub dropped: u64,
    /// Every call refused with `ENOSYS`: number and count.
    pub refused: Vec<(u64, u64)>,
    /// The calls the server answered.
    pub served: u64,
}

impl Wire for Ended {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.how.encode(e)?;
        self.code.encode(e)?;
        self.output.encode(e)?;
        self.dropped.encode(e)?;
        e.put_len(self.refused.len())?;
        for (n, c) in self.refused {
            n.encode(e)?;
            c.encode(e)?;
        }
        self.served.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let how = u8::decode(d)?;
        let code = i64::decode(d)?;
        let output = Bytes::decode(d)?;
        let dropped = u64::decode(d)?;
        let n = d.take_len()?;
        let mut refused = Vec::new();
        for _ in 0..n {
            refused.push((u64::decode(d)?, u64::decode(d)?));
        }
        Ok(Ended {
            how,
            code,
            output,
            dropped,
            refused,
            served: u64::decode(d)?,
        })
    }
}

/// A data-plane task's plane, for `run_plane` (SDD 29 f.8). The slot handles carry
/// `WRITE`, `TRANSFER` and `WAIT`: the server attaches them and waits on them for their
/// `SIGNALED` while a call waits for a batch or a window (SDD 30 f.10).
#[derive(Debug)]
pub struct Plane {
    /// Slot 0 of the task: an input slot, read on descriptor 0.
    pub input: Option<Handle>,
    /// The next slot: an output slot, written on descriptor 1.
    pub output: Option<Handle>,
    /// The kernel's end of the task's port channel.
    pub port: Option<Channel>,
    /// A memory object holding a `moruna-record` ring the engine formatted: where the
    /// task's writes to 1 and 2 go as records, one a line.
    pub log: Handle,
    /// The ring's memory object's size, whole pages.
    pub log_bytes: u64,
    /// (2.0) The account the task's process is charged to, with `READ` and `WRITE`: one
    /// the engine carved out of its own, so the kernel holds the task to its limit and
    /// the engine reads its peak (SDD 20 f.18, SDD 30 f.11).
    pub account: Handle,
    /// (3.0) Actual unconnected read-only imagefs subtree channel for /assets.
    /// None mounts no asset view. No path string is accepted as authority.
    pub assets: Option<Channel>,
}

impl Wire for Plane {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.input.encode(e)?;
        self.output.encode(e)?;
        self.port.encode(e)?;
        self.log.encode(e)?;
        self.log_bytes.encode(e)?;
        self.account.encode(e)?;
        self.assets.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Plane {
            input: Option::<Handle>::decode(d)?,
            output: Option::<Handle>::decode(d)?,
            port: Option::<Channel>::decode(d)?,
            log: Handle::decode(d)?,
            log_bytes: u64::decode(d)?,
            account: Handle::decode(d)?,
            assets: Option::<Channel>::decode(d)?,
        })
    }
}

/// The auxiliary vector entry that carries a granted program's start page (2.0, SDD 30
/// d.2): its value is the page's address, which `moruna_sys::start::init` takes. The
/// number is outside every type Linux defines, so a C library ignores it.
pub const AT_MORUNA_START: u64 = 0x4d4f_5255;

/// What a program run with `run_granted` is given besides its arguments (2.0, SDD 30
/// d.2; its budget and its scratch quota accounts since 2.0, SDD 20 f.18).
#[derive(Debug)]
pub struct Grant {
    /// Handles its process holds from its start, under slot names on its start page:
    /// the only process of the personality's that holds any (SDD 28 LX-I2 as amended).
    pub handles: Vec<(Bytes, Handle)>,
    /// (2.0) The account its process is charged to, with `READ` and `WRITE`, carved by
    /// its starter out of its own: its budget, which the kernel holds it to, and which
    /// `sysinfo` and `/proc/meminfo` say is its machine's memory.
    pub account: Handle,
    /// (2.0) The account its scratch directory's pages are charged to, with `READ` and
    /// `WRITE`: its scratch quota, which the kernel holds the directory to. `None` leaves
    /// the image filesystem's own (SDD 27 f.7).
    pub scratch: Option<Handle>,
}

impl Wire for Grant {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.handles.len())?;
        for (name, handle) in self.handles {
            name.encode(e)?;
            handle.encode(e)?;
        }
        self.account.encode(e)?;
        self.scratch.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        let mut handles = Vec::new();
        for _ in 0..n {
            handles.push((Bytes::decode(d)?, Handle::decode(d)?));
        }
        Ok(Grant {
            handles,
            account: Handle::decode(d)?,
            scratch: Option::<Handle>::decode(d)?,
        })
    }
}

/// What the log records of a `run_plane` task say (SDD 29 f.7): the record's `msg_id` is
/// the task's process name, and these headers name the stream.
pub mod log {
    /// The header naming the descriptor a line was written to.
    pub const STREAM: &str = "stream";
    pub const STDOUT: &str = "stdout";
    pub const STDERR: &str = "stderr";
}

moruna_svc::protocol! {
    /// Linux programs as processes.
    pub protocol linux {
        name = "moruna.linux";
        major = 3;
        minor = 0;
        requests {
            /// Runs the program at `path` (absolute, in the image) with `argv` and
            /// `env`, in its own process and scratch directory; answers when it ends.
            1 => fn run(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>) -> Ended;
            /// (2.0, SDD 29 f.8; its plane carries the task's account since 2.0, which is
            /// when this layout arrived) Runs the program as `run` does, as a task of the data
            /// plane: its process is given `plane`'s slots, so it may make the three
            /// calls itself, and its descriptors 0 and 1 are those slots as Arrow IPC
            /// streams (`input` read as a stream on 0, records put on `output` from the
            /// stream written to 1), for a program that speaks Arrow on its standard
            /// streams and knows no Moruna library. Whatever it writes to 2, and to 1
            /// when there is no output slot, goes to `plane.log` as records. Its process
            /// is charged to `plane.account` (2.0).
            2 => fn run_plane(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, plane: Plane) -> Ended;
            /// (2.0, SDD 30 d.2; its grant carries accounts since 2.0, which is when this
            /// layout arrived) Runs the program as `run` does, holding `grant`'s
            /// handles from its start, named on a start page whose address its auxiliary
            /// vector carries under `AT_MORUNA_START`, its process charged to
            /// `grant.account` (2.0), whose limit it is told is its machine's memory: how
            /// the supervisor starts the engine.
            3 => fn run_granted(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, grant: Grant) -> Ended;
            /// Actual mounted immutable layer seals (SDD30 image identity).
            4 => fn layers() -> Vec<Bytes>;
        }
        events {}
        errors {
            /// No file at the path.
            1 => NotFound;
            /// Not a program this personality loads: the reason is on the console.
            2 => NotExecutable;
            /// The kernel refused to make or start the process.
            3 => NoProcess;
            /// The path, arguments and environment pass `REQUEST_MAX`, or hold NUL.
            4 => BadRequest;
            /// The server runs as many programs as it can at once.
            5 => Busy;
            /// (2.0) A grant the kernel would not give: a handle without `TRANSFER`, a
            /// bad or repeated slot name, more than a start page holds.
            6 => BadGrant;
            /// The image filesystem could not report its mounted immutable layer
            /// seals. Preserves KD2's typed image-identity refusal in protocol2.0.
            7 => NoImage;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t9_ended_round_trips() {
        let ended = Ended {
            how: how::EXITED,
            code: -3,
            output: Bytes(b"hello\n".to_vec()),
            dropped: 2,
            refused: alloc::vec![(41, 1), (334, 2)],
            served: 77,
        };
        let mut e = Encoder::new();
        ended.clone().encode(&mut e).unwrap();
        let (mine, theirs) = moruna_svc::Channel::create().unwrap();
        moruna_svc::frame::send(&mine, moruna_svc::Kind::Event, 1, 0, e).unwrap();
        let mut f = moruna_svc::frame::recv(&theirs).unwrap().unwrap();
        let mut d = f.decoder();
        assert_eq!(Ended::decode(&mut d).unwrap(), ended);
        assert_eq!(d.finish(), Ok(()));
        assert_eq!(Bytes::from(&b"x"[..]), Bytes(b"x".to_vec()));
    }

    #[test]
    fn kd2_t3_a_grant_round_trips_with_its_handles() {
        let (a, _) = moruna_svc::Channel::create().unwrap();
        let (b, _) = moruna_svc::Channel::create().unwrap();
        let (account, _) = moruna_svc::Channel::create().unwrap();
        let (scratch, _) = moruna_svc::Channel::create().unwrap();
        let grant = Grant {
            handles: alloc::vec![
                (Bytes(b"vsock".to_vec()), a.0),
                (Bytes(b"linux".to_vec()), b.0)
            ],
            account: account.0,
            scratch: Some(scratch.0),
        };
        let mut e = Encoder::new();
        grant.encode(&mut e).unwrap();
        let (mine, theirs) = moruna_svc::Channel::create().unwrap();
        moruna_svc::frame::send(&mine, moruna_svc::Kind::Event, 1, 0, e).unwrap();
        let mut f = moruna_svc::frame::recv(&theirs).unwrap().unwrap();
        let mut d = f.decoder();
        let back = Grant::decode(&mut d).unwrap();
        assert_eq!(d.finish(), Ok(()));
        let names: Vec<&[u8]> = back.handles.iter().map(|(n, _)| &n.0[..]).collect();
        assert_eq!(names, [&b"vsock"[..], &b"linux"[..]]);
        assert!(back.scratch.is_some());
        assert_eq!((linux::ID.major, linux::ID.minor), (3, 0));
        const {
            assert!(
                AT_MORUNA_START > 64,
                "outside Linux's auxiliary vector types"
            )
        };
    }

    #[test]
    fn lx_t9_a_plane_round_trips_with_its_handles() {
        let (slot, _) = moruna_svc::Channel::create().unwrap();
        let (port, _) = moruna_svc::Channel::create().unwrap();
        let (log, _) = moruna_svc::Channel::create().unwrap();
        let (account, _) = moruna_svc::Channel::create().unwrap();
        let (assets, _) = moruna_svc::Channel::create().unwrap();
        let plane = Plane {
            input: Some(slot.0),
            output: None,
            port: Some(port),
            log: log.0,
            log_bytes: 65536,
            account: account.0,
            assets: Some(assets),
        };
        let mut e = Encoder::new();
        plane.encode(&mut e).unwrap();
        let (mine, theirs) = moruna_svc::Channel::create().unwrap();
        moruna_svc::frame::send(&mine, moruna_svc::Kind::Event, 1, 0, e).unwrap();
        let mut f = moruna_svc::frame::recv(&theirs).unwrap().unwrap();
        let mut d = f.decoder();
        let back = Plane::decode(&mut d).unwrap();
        assert_eq!(d.finish(), Ok(()));
        assert!(back.input.is_some() && back.output.is_none() && back.port.is_some());
        assert_eq!(back.log_bytes, 65536);
        assert!(back.assets.is_some());
    }
}

#[cfg(test)]
mod account_protocol_version_tests {
    #[test]
    fn kb5_previous_unaccounted_major_is_refused_before_requests() {
        use moruna_svc::protocol::{ProtocolId, Refusal};
        let current = super::linux::ID;
        assert_eq!((current.major, current.minor), (3, 0));
        let old = ProtocolId::new(current.name, 1, 3);
        assert_eq!(current.accepts(&old.hello()), Err(Refusal::MajorMismatch));
        assert_eq!(old.accepts(&current.hello()), Err(Refusal::MajorMismatch));
        let previous = ProtocolId::new(current.name, 2, 0);
        assert_eq!(
            current.accepts(&previous.hello()),
            Err(Refusal::MajorMismatch)
        );
        assert_eq!(current.accepts(&current.hello()), Ok(()));
    }
}
