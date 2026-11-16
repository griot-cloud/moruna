// SPDX-License-Identifier: MPL-2.0

//! A task's descriptors 0, 1 and 2 (SDD 29 f.7 and f.8, SDD 28 d.3).
//!
//! A data-plane task speaks Arrow on its standard streams and needs no Moruna library:
//! reading 0 gives its input slot's records as one Arrow IPC stream, and what it writes
//! to 1 is cut into messages, each record batch put on its output slot as a record.
//! Its text, what it writes to 2 and to 1 when it has no output slot, becomes lines in
//! the engine's log ring. A task with no plane reads 0 at its end, and its text goes
//! where the start says.
//!
//! The slots, the record layout and the ring are Moruna's (`moruna_sys::plane`,
//! `moruna-ipc-check`, `moruna-record`), which this crate does not depend on: the start
//! gives them through the ports below. These files keep the descriptors' Linux side:
//! the stream's continuity, its end marker, a message cut across writes, lines, blocking
//! and `O_NONBLOCK`.

use alloc::format;
use core::{
    fmt::Display,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    events::IoEvents,
    fs::{
        file::{AccessMode, FileCommon, FileLike, StatusFlags, file_table::FdFlags},
        pseudofs::AnonInodeFs,
    },
    prelude::*,
    process::signal::{PollHandle, Pollable},
};

/// The end-of-stream marker: the continuation and a length of zero.
pub(crate) const END: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];

/// The longest line a log record carries; a longer one is cut there.
pub(crate) const LINE_MAX: usize = 4096;

/// The most bytes written to 1 that wait for the rest of their message: a record batch
/// and its body, which the output window must hold anyway.
pub(crate) const PENDING_MAX: usize = 256 << 20;

/// The most bytes copied to a program at once.
const COPY_MAX: usize = 64 << 10;

/// The input slot.
pub(crate) trait PlaneInput: Send + Sync {
    /// Waits for the slot's next record and answers the length of the part of it that
    /// continues the stream: the record whole when `first` (its schema message, its
    /// record batch message and its body), from its record batch message otherwise.
    /// `None` once the engine has closed the slot.
    fn next(&self, first: bool) -> Result<Option<u64>>;
    /// Copies bytes `at..` of that part into `out`.
    fn read(&self, at: u64, out: &mut [u8]) -> Result<()>;
    /// Whether [`PlaneInput::next`] would answer without waiting.
    fn ready(&self) -> bool;
}

/// What an Arrow IPC message is, by its flatbuffer's header type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    all(not(ktest), not(target_os = "none")),
    expect(dead_code, reason = "the target's start makes them (`start/plane.rs`)")
)]
pub(crate) enum MessageKind {
    Schema,
    RecordBatch,
    /// Anything else, a dictionary batch included: a data-plane record carries none.
    Other,
}

/// The output slot.
pub(crate) trait PlaneOutput: Send + Sync {
    /// The kind of `message` (its prefix and flatbuffer) and the length of its body.
    fn frame(&self, message: &[u8]) -> Result<(MessageKind, usize)>;
    /// Lays `batch` and its `body` out as a record of `schema`'s stream in the slot's
    /// window and puts it, waiting for a window.
    fn put(&self, schema: &[u8], batch: &[u8], body: &[u8]) -> Result<()>;
}

/// Where a task's text goes: the engine's log ring for a data-plane task.
pub(crate) trait TextSink: Send + Sync {
    /// Takes one line written to descriptor `fd` (1 or 2), without its newline;
    /// `false` when it was dropped.
    fn line(&self, fd: u8, line: &[u8]) -> bool;
}

/// What a task's three standard descriptors are, from its start.
#[derive(Clone)]
pub(crate) struct TaskStreams {
    pub(crate) input: Option<Arc<dyn PlaneInput>>,
    pub(crate) output: Option<Arc<dyn PlaneOutput>>,
    pub(crate) text: Arc<dyn TextSink>,
    /// Lines the text sink dropped, for the task's end (`Ended::dropped`).
    pub(crate) dropped: Arc<AtomicU64>,
}

impl TaskStreams {
    /// A task with no plane whose text is `text`.
    pub(crate) fn text_only(text: Arc<dyn TextSink>) -> Self {
        Self {
            input: None,
            output: None,
            text,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The files of descriptors 0, 1 and 2.
    pub(crate) fn files(&self) -> [Arc<dyn FileLike>; 3] {
        let stdin: Arc<dyn FileLike> = Arc::new(InputFile::new(self.input.clone()));
        let stdout: Arc<dyn FileLike> = match &self.output {
            Some(output) => Arc::new(OutputFile::new(output.clone())),
            None => Arc::new(TextFile::new(1, self)),
        };
        let stderr: Arc<dyn FileLike> = Arc::new(TextFile::new(2, self));
        [stdin, stdout, stderr]
    }
}

/// Text written to the core's debug console a line at a time: a task whose start names
/// no sink, as the personality is brought up.
pub(crate) struct ConsoleText;

impl TextSink for ConsoleText {
    fn line(&self, fd: u8, line: &[u8]) -> bool {
        ostd::early_println!("[task {}] {}", fd, String::from_utf8_lossy(line));
        true
    }
}

fn fd_info(file: &dyn FileLike, fd_flags: FdFlags) -> Box<dyn Display> {
    let mut flags = file.status_flags().bits() | file.access_mode() as u32;
    if fd_flags.contains(FdFlags::CLOEXEC) {
        flags |= crate::fs::file::CreationFlags::O_CLOEXEC.bits();
    }
    Box::new(format!(
        "pos:\t0\nflags:\t0{:o}\nmnt_id:\t{}\nino:\t{}\n",
        flags,
        AnonInodeFs::mount_node().id(),
        AnonInodeFs::shared_inode().ino()
    ))
}

/// Descriptor 0: the input slot's records as one stream, then its end marker.
pub(crate) struct InputFile {
    input: Option<Arc<dyn PlaneInput>>,
    state: Mutex<InputState>,
    common: FileCommon,
}

struct InputState {
    /// No record has been given yet: the first is given with its schema message.
    first: bool,
    /// The record being given: the next byte and its end.
    at: u64,
    end: u64,
    /// The input has ended; how much of the end marker has been given.
    ended: Option<usize>,
}

impl InputFile {
    fn new(input: Option<Arc<dyn PlaneInput>>) -> Self {
        let path = AnonInodeFs::new_path(|_| "anon_inode:[moruna-plane-input]".to_string());
        Self {
            input,
            state: Mutex::new(InputState {
                first: true,
                at: 0,
                end: 0,
                ended: None,
            }),
            common: FileCommon::new(path, AccessMode::O_RDONLY, StatusFlags::empty()),
        }
    }
}

impl Pollable for InputFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        // As the message-per-call personality did (SDD 28 d.2): a descriptor is ready for
        // what it is opened for, and a read then waits for the next record.
        mask & IoEvents::IN
    }
}

impl FileLike for InputFile {
    fn read(&self, writer: &mut VmWriter) -> Result<usize> {
        let Some(input) = &self.input else {
            return Ok(0);
        };
        let mut state = self.state.lock();
        loop {
            if let Some(given) = state.ended {
                let rest = &END[given..];
                let n = rest.len().min(writer.avail());
                writer.write_fallible(&mut rest[..n].into())?;
                state.ended = Some(given + n);
                return Ok(n);
            }
            if state.at < state.end {
                break;
            }
            if self.common.is_nonblocking() && !input.ready() {
                return_errno_with_message!(Errno::EAGAIN, "the next record is not there yet");
            }
            match input.next(state.first)? {
                None => state.ended = Some(0),
                Some(len) => {
                    state.first = false;
                    state.at = 0;
                    state.end = len;
                }
            }
        }
        let n = ((state.end - state.at) as usize)
            .min(writer.avail())
            .min(COPY_MAX);
        let mut bytes = vec![0u8; n];
        input.read(state.at, &mut bytes)?;
        writer.write_fallible(&mut bytes.as_slice().into())?;
        state.at += n as u64;
        Ok(n)
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }

    fn dump_proc_fdinfo(self: Arc<Self>, fd_flags: FdFlags) -> Box<dyn Display> {
        fd_info(self.as_ref(), fd_flags)
    }
}

/// Descriptor 1 of a task with an output slot: an Arrow IPC stream, each record batch
/// put as a record.
pub(crate) struct OutputFile {
    output: Arc<dyn PlaneOutput>,
    state: Mutex<OutputState>,
    common: FileCommon,
}

#[derive(Default)]
struct OutputState {
    /// Bytes written that do not yet make a whole message.
    written: Vec<u8>,
    /// The schema message the stream began with.
    schema: Option<Vec<u8>>,
    /// The end marker was written: nothing follows on this stream.
    ended: bool,
}

impl OutputFile {
    fn new(output: Arc<dyn PlaneOutput>) -> Self {
        let path = AnonInodeFs::new_path(|_| "anon_inode:[moruna-plane-output]".to_string());
        Self {
            output,
            state: Mutex::new(OutputState::default()),
            common: FileCommon::new(path, AccessMode::O_WRONLY, StatusFlags::empty()),
        }
    }

    /// Puts every whole message written.
    fn flush(&self, state: &mut OutputState) -> Result<()> {
        loop {
            let Some((message, body)) = self.whole_message(&state.written)? else {
                return Ok(());
            };
            if message == 0 {
                state.written.clear();
                state.ended = true;
                return Ok(());
            }
            let (kind, _) = self.output.frame(&state.written[..message])?;
            match kind {
                MessageKind::Schema => {
                    if state.schema.is_some() {
                        return_errno_with_message!(Errno::EINVAL, "a second schema message");
                    }
                    state.schema = Some(state.written[..message].to_vec());
                }
                MessageKind::RecordBatch => {
                    let Some(schema) = &state.schema else {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "a record batch before its schema"
                        );
                    };
                    self.output.put(
                        schema,
                        &state.written[..message],
                        &state.written[message..message + body],
                    )?;
                }
                MessageKind::Other => {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "a data-plane record carries no dictionary"
                    );
                }
            }
            state.written.drain(..message + body);
        }
    }

    /// The length of the whole message at the start of `stream` (its prefix included)
    /// and of its body, once both are there; `(0, 0)` for the end marker.
    fn whole_message(&self, stream: &[u8]) -> Result<Option<(usize, usize)>> {
        if stream.len() < 8 {
            return Ok(None);
        }
        if stream[..4] != [0xff; 4] {
            return_errno_with_message!(Errno::EINVAL, "an IPC message without its continuation");
        }
        let len = u32::from_le_bytes([stream[4], stream[5], stream[6], stream[7]]) as usize;
        if len == 0 {
            return Ok(Some((0, 0)));
        }
        let message = 8 + len;
        if stream.len() < message {
            return Ok(None);
        }
        let (_, body) = self.output.frame(&stream[..message])?;
        if stream.len() < message + body {
            return Ok(None);
        }
        Ok(Some((message, body)))
    }
}

impl Pollable for OutputFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        mask & IoEvents::OUT
    }
}

impl FileLike for OutputFile {
    fn write(&self, reader: &mut VmReader) -> Result<usize> {
        let mut state = self.state.lock();
        if state.ended {
            return_errno_with_message!(Errno::EPIPE, "the stream has ended");
        }
        let len = reader.remain();
        if state.written.len() + len > PENDING_MAX {
            return_errno_with_message!(Errno::EFBIG, "a message larger than the window");
        }
        let from = state.written.len();
        state.written.resize(from + len, 0);
        let copied = reader.read_fallible(&mut VmWriter::from(&mut state.written[from..]));
        if let Err((error, n)) = copied {
            state.written.truncate(from + n);
            if n == 0 {
                return Err(error.into());
            }
        }
        let taken = state.written.len() - from;
        self.flush(&mut state)?;
        Ok(taken)
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }

    fn dump_proc_fdinfo(self: Arc<Self>, fd_flags: FdFlags) -> Box<dyn Display> {
        fd_info(self.as_ref(), fd_flags)
    }
}

/// Descriptor 2, and 1 without an output slot: text, a line at a time.
pub(crate) struct TextFile {
    fd: u8,
    text: Arc<dyn TextSink>,
    dropped: Arc<AtomicU64>,
    line: Mutex<Vec<u8>>,
    common: FileCommon,
}

impl TextFile {
    fn new(fd: u8, streams: &TaskStreams) -> Self {
        let path = AnonInodeFs::new_path(|_| "anon_inode:[moruna-text]".to_string());
        Self {
            fd,
            text: streams.text.clone(),
            dropped: streams.dropped.clone(),
            line: Mutex::new(Vec::new()),
            common: FileCommon::new(path, AccessMode::O_WRONLY, StatusFlags::empty()),
        }
    }

    fn push(&self, line: &[u8]) {
        if !self.text.line(self.fd, line) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for TextFile {
    fn drop(&mut self) {
        // The line left without its newline at the last close.
        let line = core::mem::take(&mut *self.line.lock());
        if !line.is_empty() {
            self.push(&line);
        }
    }
}

impl Pollable for TextFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        mask & IoEvents::OUT
    }
}

impl FileLike for TextFile {
    fn write(&self, reader: &mut VmReader) -> Result<usize> {
        let len = reader.remain();
        let mut bytes = vec![0u8; len];
        if let Err((error, n)) = reader.read_fallible(&mut VmWriter::from(bytes.as_mut_slice())) {
            if n == 0 {
                return Err(error.into());
            }
            bytes.truncate(n);
        }
        let mut line = self.line.lock();
        for &byte in &bytes {
            if byte == b'\n' {
                let whole = core::mem::take(&mut *line);
                self.push(&whole);
            } else if line.len() < LINE_MAX {
                line.push(byte);
            }
        }
        Ok(bytes.len())
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }

    fn dump_proc_fdinfo(self: Arc<Self>, fd_flags: FdFlags) -> Box<dyn Display> {
        fd_info(self.as_ref(), fd_flags)
    }
}

#[cfg(ktest)]
mod test {
    use core::sync::atomic::AtomicBool;

    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::file::{CreationFlags, StatusFlagsUpdate};

    /// An input slot holding `records`, each the stream part `next` answers.
    struct Slot {
        records: Mutex<VecDeque<Vec<u8>>>,
        current: Mutex<Vec<u8>>,
        firsts: Mutex<Vec<bool>>,
        ready: AtomicBool,
    }

    impl Slot {
        fn new(records: &[&[u8]]) -> Arc<Self> {
            Arc::new(Self {
                records: Mutex::new(records.iter().map(|r| r.to_vec()).collect()),
                current: Mutex::new(vec![]),
                firsts: Mutex::new(vec![]),
                ready: AtomicBool::new(true),
            })
        }
    }

    impl PlaneInput for Slot {
        fn next(&self, first: bool) -> Result<Option<u64>> {
            self.firsts.lock().push(first);
            let Some(record) = self.records.lock().pop_front() else {
                return Ok(None);
            };
            let len = record.len() as u64;
            *self.current.lock() = record;
            Ok(Some(len))
        }

        fn read(&self, at: u64, out: &mut [u8]) -> Result<()> {
            let current = self.current.lock();
            out.copy_from_slice(&current[at as usize..at as usize + out.len()]);
            Ok(())
        }

        fn ready(&self) -> bool {
            self.ready.load(Ordering::Relaxed)
        }
    }

    /// An output slot: a message's first byte after its prefix is its kind (`S`, `B`,
    /// `D`) and its second its body's length.
    /// A record put: its schema, its batch and its body.
    type Put = (Vec<u8>, Vec<u8>, Vec<u8>);

    struct Window {
        put: Mutex<Vec<Put>>,
    }

    impl Default for Window {
        fn default() -> Self {
            Self {
                put: Mutex::new(vec![]),
            }
        }
    }

    impl PlaneOutput for Window {
        fn frame(&self, message: &[u8]) -> Result<(MessageKind, usize)> {
            let kind = match message.get(8) {
                Some(b'S') => MessageKind::Schema,
                Some(b'B') => MessageKind::RecordBatch,
                _ => MessageKind::Other,
            };
            Ok((kind, *message.get(9).unwrap_or(&0) as usize))
        }

        fn put(&self, schema: &[u8], batch: &[u8], body: &[u8]) -> Result<()> {
            if body == b"full" {
                return_errno_with_message!(Errno::EFBIG, "no window holds it");
            }
            self.put
                .lock()
                .push((schema.to_vec(), batch.to_vec(), body.to_vec()));
            Ok(())
        }
    }

    struct Lines {
        lines: Mutex<Vec<(u8, Vec<u8>)>>,
        refuse: AtomicBool,
    }

    impl Default for Lines {
        fn default() -> Self {
            Self {
                lines: Mutex::new(vec![]),
                refuse: AtomicBool::new(false),
            }
        }
    }

    impl TextSink for Lines {
        fn line(&self, fd: u8, line: &[u8]) -> bool {
            if self.refuse.load(Ordering::Relaxed) {
                return false;
            }
            self.lines.lock().push((fd, line.to_vec()));
            true
        }
    }

    /// A message: the continuation, its length, its kind and its body's length.
    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xff, 0xff, 0xff, 8, 0, 0, 0, kind, body.len() as u8];
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(body);
        bytes
    }

    fn read_all(file: &dyn FileLike, chunk: usize) -> Vec<u8> {
        let mut all = vec![];
        loop {
            let mut buf = vec![0u8; chunk];
            let n = file
                .read(&mut VmWriter::from(buf.as_mut_slice()).to_fallible())
                .unwrap();
            if n == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..n]);
        }
    }

    fn write(file: &dyn FileLike, bytes: &[u8]) -> Result<usize> {
        file.write(&mut VmReader::from(bytes).to_fallible())
    }

    fn errno<T>(result: Result<T>) -> Errno {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.error(),
        }
    }

    fn streams(
        input: Option<Arc<dyn PlaneInput>>,
        output: Option<Arc<dyn PlaneOutput>>,
        text: Arc<Lines>,
    ) -> TaskStreams {
        TaskStreams {
            input,
            output,
            text,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    #[ktest]
    fn descriptor_0_is_the_slot_s_records_then_the_end() {
        let slot = Slot::new(&[b"schema+batch1", b"", b"batch2"]);
        let files = streams(Some(slot.clone()), None, Arc::new(Lines::default())).files();
        let stdin = &files[0];

        let mut stream = read_all(stdin.as_ref(), 5);
        let end = stream.split_off(stream.len() - 8);
        assert_eq!(stream, b"schema+batch1batch2");
        assert_eq!(end, END);
        // The first record is asked for whole, the rest from their batch.
        assert_eq!(*slot.firsts.lock(), [true, false, false, false]);
        // After the end, nothing more.
        assert!(read_all(stdin.as_ref(), 5).is_empty());

        assert_eq!(stdin.poll(IoEvents::IN | IoEvents::OUT, None), IoEvents::IN);
        assert_eq!(errno(write(stdin.as_ref(), b"x")), Errno::EBADF);
        assert_eq!(stdin.access_mode(), AccessMode::O_RDONLY);
    }

    #[ktest]
    fn a_nonblocking_read_does_not_wait_for_a_record() {
        let slot = Slot::new(&[b"abc"]);
        let file = InputFile::new(Some(slot.clone()));
        (&file as &dyn FileLike)
            .update_status_flags(StatusFlagsUpdate::set(StatusFlags::O_NONBLOCK))
            .unwrap();
        slot.ready.store(false, Ordering::Relaxed);
        let mut buf = [0u8; 3];
        assert_eq!(
            errno(file.read(&mut VmWriter::from(buf.as_mut_slice()).to_fallible())),
            Errno::EAGAIN
        );
        slot.ready.store(true, Ordering::Relaxed);
        assert_eq!(
            file.read(&mut VmWriter::from(buf.as_mut_slice()).to_fallible())
                .unwrap(),
            3
        );
    }

    #[ktest]
    fn without_an_input_slot_descriptor_0_is_at_its_end() {
        let files = TaskStreams::text_only(Arc::new(ConsoleText)).files();
        assert!(read_all(files[0].as_ref(), 8).is_empty());
        assert!(ConsoleText.line(1, b"hello"));
    }

    #[ktest]
    fn descriptor_1_puts_each_batch_as_a_record() {
        let window = Arc::new(Window::default());
        let files = streams(None, Some(window.clone()), Arc::new(Lines::default())).files();
        let stdout = &files[1];
        let schema = message(b'S', b"");
        let batch1 = message(b'B', b"body1");
        let batch2 = message(b'B', b"b2");

        // A message cut across writes, then two in one.
        let mut stream = schema.clone();
        stream.extend_from_slice(&batch1);
        let (a, b) = stream.split_at(13);
        assert_eq!(write(stdout.as_ref(), a).unwrap(), 13);
        assert_eq!(write(stdout.as_ref(), &b[..3]).unwrap(), 3);
        assert!(window.put.lock().is_empty());
        assert_eq!(write(stdout.as_ref(), &b[3..]).unwrap(), b.len() - 3);
        let mut rest = batch2.clone();
        rest.extend_from_slice(&END);
        assert_eq!(write(stdout.as_ref(), &rest).unwrap(), rest.len());

        let put = window.put.lock();
        assert_eq!(put.len(), 2);
        assert_eq!(put[0].0, schema);
        assert_eq!(put[0].1, batch1[..16]);
        assert_eq!(put[0].2, b"body1");
        assert_eq!(put[1].2, b"b2");
        drop(put);

        // Nothing follows the end.
        assert_eq!(errno(write(stdout.as_ref(), &schema)), Errno::EPIPE);
        assert_eq!(
            stdout.poll(IoEvents::IN | IoEvents::OUT, None),
            IoEvents::OUT
        );
        let mut buf = [0u8; 1];
        assert_eq!(
            errno(stdout.read(&mut VmWriter::from(buf.as_mut_slice()).to_fallible())),
            Errno::EBADF
        );
    }

    #[ktest]
    fn descriptor_1_refuses_what_is_not_a_plane_stream() {
        let output = || OutputFile::new(Arc::new(Window::default()) as Arc<dyn PlaneOutput>);
        let schema = message(b'S', b"");

        // A batch before its schema, a second schema, a dictionary.
        assert_eq!(errno(write(&output(), &message(b'B', b"x"))), Errno::EINVAL);
        let file = output();
        write(&file, &schema).unwrap();
        assert_eq!(errno(write(&file, &schema)), Errno::EINVAL);
        let file = output();
        write(&file, &schema).unwrap();
        assert_eq!(errno(write(&file, &message(b'D', b""))), Errno::EINVAL);

        // A message without its continuation, and one no window holds.
        assert_eq!(errno(write(&output(), &[1u8; 8])), Errno::EINVAL);
        let file = output();
        write(&file, &schema).unwrap();
        assert_eq!(errno(write(&file, &message(b'B', b"full"))), Errno::EFBIG);

        // More than a window waiting for the rest of its message.
        let file = output();
        file.state.lock().written = vec![0xff; PENDING_MAX - 2];
        assert_eq!(errno(write(&file, &[0xff; 3])), Errno::EFBIG);
    }

    #[ktest]
    fn text_is_lines() {
        let lines = Arc::new(Lines::default());
        let streams = streams(None, None, lines.clone());
        let [_, stdout, stderr] = streams.files();
        write(stdout.as_ref(), b"one\ntw").unwrap();
        write(stdout.as_ref(), b"o\n").unwrap();
        write(stderr.as_ref(), &[b'x'; LINE_MAX + 10]).unwrap();
        write(stderr.as_ref(), b"\nlast").unwrap();
        assert_eq!(
            stderr.poll(IoEvents::IN | IoEvents::OUT, None),
            IoEvents::OUT
        );

        // The line left without its newline goes at the last close.
        drop(stderr);
        let got = lines.lines.lock().clone();
        assert_eq!(got[0], (1, b"one".to_vec()));
        assert_eq!(got[1], (1, b"two".to_vec()));
        assert_eq!(got[2].0, 2);
        assert_eq!(got[2].1.len(), LINE_MAX);
        assert_eq!(got[3], (2, b"last".to_vec()));

        // A line the sink drops is counted.
        lines.refuse.store(true, Ordering::Relaxed);
        write(stdout.as_ref(), b"lost\n").unwrap();
        assert_eq!(streams.dropped.load(Ordering::Relaxed), 1);
    }

    #[ktest]
    fn fdinfo_names_the_flags() {
        let files = TaskStreams::text_only(Arc::new(ConsoleText)).files();
        let [stdin, stdout, stderr] = files;
        let info = stdin.clone().dump_proc_fdinfo(FdFlags::CLOEXEC).to_string();
        assert!(info.starts_with("pos:\t0\nflags:\t0"));
        let cloexec = CreationFlags::O_CLOEXEC.bits();
        let flags = u32::from_str_radix(
            info.lines().nth(1).unwrap().trim_start_matches("flags:\t0"),
            8,
        )
        .unwrap();
        assert_ne!(flags & cloexec, 0);
        assert!(
            stdout
                .dump_proc_fdinfo(FdFlags::empty())
                .to_string()
                .contains("ino:")
        );
        assert!(
            stderr
                .dump_proc_fdinfo(FdFlags::empty())
                .to_string()
                .contains("mnt_id:")
        );

        let output = Arc::new(OutputFile::new(Arc::new(Window::default())));
        assert!(
            output
                .dump_proc_fdinfo(FdFlags::empty())
                .to_string()
                .contains("ino:")
        );
    }
}
