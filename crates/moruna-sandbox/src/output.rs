//! A `.pex` child's own output (MH 4.3 `log`): three pipes per child, made before the fork,
//! whose write ends the child puts in place of its standard output and error and gives its
//! logging records to, and whose read ends Moruna reads on threads of their own until the child
//! is gone. Each child is one stage's, so everything on its pipes is that stage's; nothing it
//! writes there is read as anything but its output.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use moruna_kernel::StageId;
use moruna_kernel::output::{Level, OutputSink, Stream};

/// How much a reader takes at once.
pub const READ_CHUNK: usize = 64 << 10;
/// The longest logging record read as one; a longer line is the child's standard error, in
/// pieces.
pub const RECORD_LINE_MAX: usize = (1 << 20) + 4096;

/// The child's three pipes, before the fork.
pub struct ChildOutput {
    pipes: [(OwnedFd, OwnedFd); 3],
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0 as RawFd; 2];
    // SAFETY: `ends` is two descriptors' room; pipe2 writes both or neither.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just made and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

impl ChildOutput {
    /// Standard output, standard error and logging records: three pipes, close-on-exec.
    pub fn open() -> io::Result<ChildOutput> {
        Ok(ChildOutput {
            pipes: [pipe()?, pipe()?, pipe()?],
        })
    }

    /// The write ends, for the template to hand to the child.
    pub fn write_ends(&self) -> [RawFd; 3] {
        [
            self.pipes[0].1.as_raw_fd(),
            self.pipes[1].1.as_raw_fd(),
            self.pipes[2].1.as_raw_fd(),
        ]
    }

    /// The child has its write ends: close ours and read the pipes until the child is gone,
    /// into `to` as its stage's (holding the sink till the last byte), or, with no sink, onto
    /// this process's standard error as before any sink existed.
    pub fn read(self, to: Option<(Arc<dyn OutputSink>, StageId)>, kernel: &str) {
        let [(out, out_w), (err, err_w), (records, records_w)] = self.pipes;
        drop((out_w, err_w, records_w));
        let named = |what: &str| format!("moruna-{kernel}-{what}");
        for (fd, what, stream) in [
            (out, "stdout", Stream::Stdout),
            (err, "stderr", Stream::Stderr),
        ] {
            let to = to.clone();
            let spawned = std::thread::Builder::new()
                .name(named(what))
                .spawn(move || bytes(std::fs::File::from(fd), stream, to));
            if let Err(error) = spawned {
                tracing::error!(target: "sandbox", %error, kernel, "no thread for a child's {what}");
            }
        }
        let spawned = std::thread::Builder::new()
            .name(named("records"))
            .spawn(move || lines(std::fs::File::from(records), to));
        if let Err(error) = spawned {
            tracing::error!(target: "sandbox", %error, kernel, "no thread for a child's records");
        }
    }
}

fn bytes(mut from: std::fs::File, stream: Stream, to: Option<(Arc<dyn OutputSink>, StageId)>) {
    let _hold = to.as_ref().map(|(sink, _)| sink.hold());
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => match &to {
                Some((sink, stage)) => sink.bytes(Some(*stage), stream, &buf[..n]),
                None => {
                    let _ = io::stderr().write_all(&buf[..n]);
                }
            },
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// One logging record as the child writes it: `{"level": <Python level>, "text": "..."}`.
#[derive(serde::Deserialize)]
struct Record {
    level: i64,
    text: String,
}

fn lines(from: std::fs::File, to: Option<(Arc<dyn OutputSink>, StageId)>) {
    let _hold = to.as_ref().map(|(sink, _)| sink.hold());
    let mut reader = BufReader::with_capacity(READ_CHUNK, from);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut over = false;
        loop {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            if available.is_empty() {
                if line.is_empty() {
                    return;
                }
                break;
            }
            let (take, done) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len(), false),
            };
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if line.len() > RECORD_LINE_MAX {
                // Not a record the child's handler wrote: its bytes are the child's error output.
                as_error_output(&to, &line);
                line.clear();
                over = true;
            }
            if done {
                break;
            }
        }
        if over && line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Record>(&line) {
            Ok(record) => match &to {
                Some((sink, stage)) => sink.record(
                    Some(*stage),
                    Some(Level::of_python(record.level)),
                    &record.text,
                ),
                None => {
                    let _ = writeln!(io::stderr(), "{}", record.text);
                }
            },
            Err(_) => as_error_output(&to, &line),
        }
    }
}

/// Bytes on the records pipe that are no record: the child's error output, never a level.
fn as_error_output(to: &Option<(Arc<dyn OutputSink>, StageId)>, bytes: &[u8]) {
    match to {
        Some((sink, stage)) => sink.bytes(Some(*stage), Stream::Stderr, bytes),
        None => {
            let _ = io::stderr().write_all(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Got = (Option<StageId>, String, Option<Level>);

    #[derive(Default)]
    struct Seen {
        got: Mutex<Vec<Got>>,
        holds: std::sync::atomic::AtomicUsize,
    }

    impl OutputSink for Seen {
        fn bytes(&self, stage: Option<StageId>, stream: Stream, bytes: &[u8]) {
            self.got.lock().unwrap().push((
                stage,
                format!("{stream:?}:{}", String::from_utf8_lossy(bytes)),
                None,
            ));
        }
        fn record(&self, stage: Option<StageId>, level: Option<Level>, text: &str) {
            self.got
                .lock()
                .unwrap()
                .push((stage, format!("Record:{text}"), level));
        }
        fn hold(&self) -> Box<dyn Send> {
            self.holds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(())
        }
    }

    /// What a child writes on its three pipes is its stage's: bytes as they are, a record with
    /// its level, a line on the records pipe that is no record as error output, never levelled.
    #[test]
    fn a_childs_pipes_are_its_stages() {
        let seen = Arc::new(Seen::default());
        let output = ChildOutput::open().unwrap();
        let [out, err, records] = output.write_ends();
        // SAFETY: duplicates of descriptors the test owns, closed by their Files.
        let (mut out, mut err, mut records) = unsafe {
            (
                std::fs::File::from_raw_fd(libc::dup(out)),
                std::fs::File::from_raw_fd(libc::dup(err)),
                std::fs::File::from_raw_fd(libc::dup(records)),
            )
        };
        output.read(Some((seen.clone() as Arc<dyn OutputSink>, 3)), "k");
        out.write_all(b"hello\n").unwrap();
        err.write_all(b"oops\n").unwrap();
        records
            .write_all(b"{\"level\": 40, \"text\": \"boom\\nTraceback\"}\nnot a record\n")
            .unwrap();
        drop((out, err, records));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.got.lock().unwrap().len() < 4 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut got = seen.got.lock().unwrap().clone();
        got.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            got,
            vec![
                (
                    Some(3),
                    "Record:boom\nTraceback".to_string(),
                    Some(Level::Error)
                ),
                (Some(3), "Stderr:not a record\n".to_string(), None),
                (Some(3), "Stderr:oops\n".to_string(), None),
                (Some(3), "Stdout:hello\n".to_string(), None),
            ]
        );
        assert_eq!(seen.holds.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
