//! The control protocol (17 e.3): little-endian `u32` length, then that many bytes of JSON, one
//! object with an `op` field. A control message is never a batch (SB-I7).

use std::io::{Read, Write};

use moruna_kernel::{AllocCounts, KernelAlloc};
use serde::{Deserialize, Serialize};

/// The largest frame either side accepts (17 i).
pub const FRAME_MAX_BYTES: usize = 1 << 20;

/// A refusal the child's gate made (05 b), as it crosses back.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalDoc {
    /// Bytes the request asked for.
    pub requested: u64,
    /// The child's memory when it was judged.
    pub in_use: u64,
    /// The stage ceiling it would have passed.
    pub ceiling: u64,
}

/// A call's counts as they cross back (17 e.3): each source `[bytes, requests, largest, peak,
/// refused]`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocDoc {
    /// Whether the call was counted.
    pub measured: bool,
    /// Whether the guard could refuse its requests.
    pub refusal_on: bool,
    /// Python objects.
    pub python: [u64; 5],
    /// NumPy data.
    pub numpy: [u64; 5],
    /// Arrow, from the child's own pool.
    pub arrow: [u64; 5],
}

fn counts_of(c: &AllocCounts) -> [u64; 5] {
    [c.bytes, c.requests, c.largest, c.peak, c.refused]
}

fn counts(a: [u64; 5]) -> AllocCounts {
    AllocCounts {
        bytes: a[0],
        requests: a[1],
        largest: a[2],
        peak: a[3],
        refused: a[4],
    }
}

impl From<&KernelAlloc> for AllocDoc {
    fn from(a: &KernelAlloc) -> AllocDoc {
        AllocDoc {
            measured: a.measured,
            refusal_on: a.refusal_on,
            python: counts_of(&a.python),
            numpy: counts_of(&a.numpy),
            arrow: counts_of(&a.arrow),
        }
    }
}

impl From<&AllocDoc> for KernelAlloc {
    fn from(a: &AllocDoc) -> KernelAlloc {
        KernelAlloc {
            measured: a.measured,
            refusal_on: a.refusal_on,
            python: counts(a.python),
            numpy: counts(a.numpy),
            arrow: counts(a.arrow),
            linear: AllocCounts::default(),
        }
    }
}

/// Moruna to the template (17 e.3); the request carries the child's control socket and the memfd.
// One `start` is built per stage and dropped once sent: its size is never multiplied.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToTemplate {
    /// Fork a child for a stage.
    Start {
        /// The package's tree.
        tree: String,
        /// The `deps/` directory to put on the path.
        target: String,
        /// `<module>:<attribute>`.
        entry: String,
        /// The package's manifest.
        manifest: serde_json::Value,
        /// The stage's cgroup, created by Moruna.
        cgroup: String,
        /// An empty directory the child mounts its root on.
        root: String,
        /// The mapping's length.
        mapping_bytes: u64,
        /// The libraries the template imported, which the package may not carry at another
        /// version (17 f.6).
        libraries: std::collections::BTreeMap<String, String>,
        /// The arguments of a kernel built by them (16 b): the factory's.
        args: Option<serde_json::Value>,
        /// The assets directory, mounted read only at `/assets` (17 f.11).
        assets: Option<String>,
    },
    /// How a child ended (SB-I11).
    Status {
        /// The child.
        pid: u32,
    },
}

/// The template to Moruna.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromTemplate {
    /// Once, when the template is ready.
    Hello {
        /// The template's pid.
        pid: u32,
        /// Its interpreter's version, `3.14.7`.
        python: String,
        /// The libraries it imported, by distribution name, with their versions.
        libraries: std::collections::BTreeMap<String, String>,
        /// Its C library's version, `[major, minor]`; absent without glibc.
        glibc: Option<[u32; 2]>,
        /// The machine, `x86_64` or `aarch64`.
        machine: String,
    },
    /// A child was forked.
    Started {
        /// Its pid.
        pid: u32,
    },
    /// How a child ended: `exit status 3`, `killed by signal 9`.
    Exited {
        /// The child.
        pid: u32,
        /// How.
        status: String,
    },
    /// The request was refused.
    Refused {
        /// Why.
        reason: String,
    },
}

/// Moruna to the child.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToChild {
    /// Run one batch: an Arrow IPC stream at `offset` in the mapping.
    Call {
        /// Where the input is.
        offset: u64,
        /// Its length.
        len: u64,
    },
    /// Open the state at `offset` (a resume).
    Open {
        /// Where the state is.
        offset: u64,
        /// Its length.
        len: u64,
    },
    /// Write the kernel's state into the mapping.
    Checkpoint,
    /// Exit.
    Stop,
}

/// The child to Moruna.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromChild {
    /// The package is loaded.
    Ready,
    /// The package could not be loaded, or confinement failed.
    Failed {
        /// The step and the error.
        reason: String,
    },
    /// A call's output.
    Done {
        /// Where it is.
        offset: u64,
        /// Its length.
        len: u64,
        /// The call's counts.
        alloc: AllocDoc,
    },
    /// A call failed.
    Error {
        /// The kernel's error.
        message: String,
        /// The guard's refusal, when one ended the call.
        refusal: Option<RefusalDoc>,
        /// The call's counts.
        alloc: AllocDoc,
    },
    /// The state, for a checkpoint.
    State {
        /// Where it is.
        offset: u64,
        /// Its length.
        len: u64,
    },
    /// `open` succeeded.
    Opened,
}

/// Why a frame could not be read or written.
#[derive(Debug)]
pub enum FrameError {
    /// The peer closed the stream before a frame began.
    Closed,
    /// The stream failed, or ended inside a frame.
    Io(std::io::Error),
    /// The frame is over [`FRAME_MAX_BYTES`].
    TooLarge(usize),
    /// The frame is not the JSON of a message.
    Malformed(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Closed => write!(f, "the peer closed the control socket"),
            FrameError::Io(e) => write!(f, "the control socket failed: {e}"),
            FrameError::TooLarge(n) => {
                write!(
                    f,
                    "a frame of {n} bytes is over the {FRAME_MAX_BYTES} allowed"
                )
            }
            FrameError::Malformed(e) => write!(f, "a frame that is not a message: {e}"),
        }
    }
}

/// Write one message as a frame.
pub fn write_frame<T: Serialize>(out: &mut impl Write, message: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if body.len() > FRAME_MAX_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame).map_err(FrameError::Io)?;
    out.flush().map_err(FrameError::Io)
}

/// Read one frame's body.
pub fn read_body(input: &mut impl Read) -> Result<Vec<u8>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match input.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Err(FrameError::Closed),
            Ok(0) => {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the stream ended inside a frame's length",
                )));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > FRAME_MAX_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).map_err(FrameError::Io)?;
    Ok(body)
}

/// Read one message.
pub fn read_frame<T: for<'de> Deserialize<'de>>(input: &mut impl Read) -> Result<T, FrameError> {
    let body = read_body(input)?;
    serde_json::from_slice(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(m: T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);
        assert_eq!(read_frame::<T>(&mut buf.as_slice()).unwrap(), m);
    }

    /// SB-T8: every message of e.3 round-trips; oversized and malformed frames are refused.
    #[test]
    fn sb_t8_protocol_frames() {
        round(ToTemplate::Start {
            tree: "/t".into(),
            target: "cp314-manylinux_2_28_x86_64".into(),
            entry: "function:f".into(),
            manifest: serde_json::json!({"name": "f"}),
            cgroup: "/sys/fs/cgroup/x".into(),
            root: "/tmp/r".into(),
            mapping_bytes: 1 << 20,
            libraries: [("numpy".to_string(), "2.3.0".to_string())].into(),
            args: Some(serde_json::json!({"k": 1})),
            assets: Some("/disk/1/models".into()),
        });
        round(ToTemplate::Status { pid: 4 });
        round(FromTemplate::Exited {
            pid: 4,
            status: "exit status 3".into(),
        });
        round(FromTemplate::Hello {
            pid: 1,
            python: "3.14.7".into(),
            libraries: [("pyarrow".to_string(), "25.0.1".to_string())].into(),
            glibc: Some([2, 41]),
            machine: "aarch64".into(),
        });
        round(FromTemplate::Started { pid: 9 });
        round(FromTemplate::Refused { reason: "x".into() });
        round(ToChild::Call {
            offset: 4096,
            len: 10,
        });
        round(ToChild::Open { offset: 1, len: 2 });
        round(ToChild::Checkpoint);
        round(ToChild::Stop);
        round(FromChild::Ready);
        round(FromChild::Opened);
        round(FromChild::Failed {
            reason: "landlock".into(),
        });
        let mut alloc = KernelAlloc {
            measured: true,
            refusal_on: true,
            ..KernelAlloc::default()
        };
        alloc.numpy.bytes = 7;
        alloc.arrow.peak = 3;
        alloc.python.refused = 1;
        let doc = AllocDoc::from(&alloc);
        assert_eq!(KernelAlloc::from(&doc), alloc);
        round(FromChild::Done {
            offset: 1,
            len: 2,
            alloc: doc,
        });
        round(FromChild::Error {
            message: "MemoryError".into(),
            refusal: Some(RefusalDoc {
                requested: 1,
                in_use: 2,
                ceiling: 3,
            }),
            alloc: doc,
        });
        round(FromChild::State { offset: 1, len: 2 });

        // Over the limit, both ways.
        let mut big = Vec::new();
        big.extend_from_slice(&((FRAME_MAX_BYTES + 1) as u32).to_le_bytes());
        assert!(matches!(
            read_body(&mut big.as_slice()),
            Err(FrameError::TooLarge(_))
        ));
        let huge = FromChild::Failed {
            reason: "x".repeat(FRAME_MAX_BYTES + 1),
        };
        assert!(matches!(
            write_frame(&mut Vec::new(), &huge),
            Err(FrameError::TooLarge(_))
        ));
        // Not JSON, an unknown op, a cut frame, an empty stream.
        let mut junk = 4u32.to_le_bytes().to_vec();
        junk.extend_from_slice(b"junk");
        let e = read_frame::<ToChild>(&mut junk.as_slice()).unwrap_err();
        assert!(e.to_string().contains("not a message"));
        let mut unknown = Vec::new();
        write_frame(&mut unknown, &serde_json::json!({"op": "dance"})).unwrap();
        assert!(read_frame::<ToChild>(&mut unknown.as_slice()).is_err());
        let mut cut = 10u32.to_le_bytes().to_vec();
        cut.extend_from_slice(b"abc");
        assert!(matches!(
            read_body(&mut cut.as_slice()),
            Err(FrameError::Io(_))
        ));
        assert!(matches!(
            read_body(&mut [1u8, 0].as_slice()),
            Err(FrameError::Io(_))
        ));
        assert!(matches!(
            read_body(&mut [].as_slice()),
            Err(FrameError::Closed)
        ));
        assert!(FrameError::Closed.to_string().contains("closed"));
        assert!(
            FrameError::Io(std::io::Error::other("x"))
                .to_string()
                .contains("failed")
        );
    }
}
