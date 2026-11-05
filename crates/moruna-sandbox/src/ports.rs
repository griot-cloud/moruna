//! The port protocol (version 1): how an image task's process calls the ports its needs are
//! (SDD 19 f.6, MH 4.10). A need is a named request and response the runner serves; the process
//! reaches every one of them over one connected stream socket the runner gives it, which its
//! environment names (`MORUNA_PORT_SOCKET`: `fd:N`, a descriptor it inherits, or `unix:PATH`).
//!
//! The process sends one request and reads its answer before it sends the next:
//!
//! ```text
//! request   "MRP1"  u16 name length  name (UTF-8)  u64 body length  body
//! answer    u8 status  u64 length  bytes
//! ```
//!
//! Integers are little-endian. Status 0 is the port's answer, its bytes as the port gave them;
//! status 1 is a refusal, its bytes the reason in UTF-8 (the port refused, the name is not one of
//! the task's needs, the request is too large, the call's deadline passed). A port that answers
//! with batches answers with an Arrow IPC stream. The process ends the conversation by closing
//! its end; anything that is not a request ends it too, and the runner says why on the task's
//! standard error. The protocol is the same on a Linux node, where the runner holds the other
//! end in its own process, and under any OCI runtime a host gives the socket to.

use std::io::{self, Read, Write};
use std::sync::Arc;

use moruna_kernel::{Answer, StagePorts};

/// The four bytes every request starts with.
pub const REQUEST_MAGIC: &[u8; 4] = b"MRP1";
/// The environment variable that names the socket.
pub const SOCKET_ENV: &str = "MORUNA_PORT_SOCKET";
/// The environment variable that lists the task's port names, a JSON array in name order.
pub const PORTS_ENV: &str = "MORUNA_PORTS";
/// The status of an answer.
pub const ANSWERED: u8 = 0;
/// The status of a refusal.
pub const REFUSED: u8 = 1;
/// The longest name a request carries.
pub const NAME_MAX: usize = 64;

/// One request read from `stream`: the port's name and the body; `None` when the process
/// closed its end between requests.
pub fn read_request(stream: &mut dyn Read) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut magic = [0u8; 4];
    match read_full(stream, &mut magic)? {
        0 => return Ok(None),
        4 => {}
        n => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("a request ended after {n} bytes of its magic"),
            ));
        }
    }
    if &magic != REQUEST_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a request starts with {magic:?}, not \"MRP1\": the process does not speak the \
                 port protocol, version 1"
            ),
        ));
    }
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let len = usize::from(u16::from_le_bytes(len));
    if len == 0 || len > NAME_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a request names a port of {len} bytes; a name is 1 to {NAME_MAX}"),
        ));
    }
    let mut name = vec![0u8; len];
    stream.read_exact(&mut name)?;
    let name = String::from_utf8(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "a request's port name is not UTF-8",
        )
    })?;
    let mut body_len = [0u8; 8];
    stream.read_exact(&mut body_len)?;
    let body_len = u64::from_le_bytes(body_len);
    if body_len > moruna_kernel::ports::PORT_BYTES_MAX as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a request to `{name}` is {body_len} bytes, more than the {} a call carries",
                moruna_kernel::ports::PORT_BYTES_MAX
            ),
        ));
    }
    let mut body = vec![0u8; body_len as usize];
    stream.read_exact(&mut body)?;
    Ok(Some((name, body)))
}

fn read_full(stream: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match stream.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Write one answer.
pub fn write_answer(stream: &mut dyn Write, status: u8, bytes: &[u8]) -> io::Result<()> {
    let mut head = [0u8; 9];
    head[0] = status;
    head[1..].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    stream.write_all(&head)?;
    stream.write_all(bytes)?;
    stream.flush()
}

/// Write one request, as a process does: for a client of the protocol and its tests.
pub fn write_request(stream: &mut dyn Write, name: &str, body: &[u8]) -> io::Result<()> {
    let len = u16::try_from(name.len())
        .ok()
        .filter(|l| (1..=NAME_MAX as u16).contains(l))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a port name is 1 to 64 bytes")
        })?;
    stream.write_all(REQUEST_MAGIC)?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(name.as_bytes())?;
    stream.write_all(&(body.len() as u64).to_le_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Read one answer: `Ok(bytes)` for an answer, `Err(reason)` for a refusal.
pub fn read_answer(stream: &mut dyn Read) -> io::Result<Result<Vec<u8>, String>> {
    let mut head = [0u8; 9];
    stream.read_exact(&mut head)?;
    let mut len = [0u8; 8];
    len.copy_from_slice(&head[1..]);
    let mut bytes = vec![0u8; u64::from_le_bytes(len) as usize];
    stream.read_exact(&mut bytes)?;
    match head[0] {
        ANSWERED => Ok(Ok(bytes)),
        REFUSED => Ok(Err(String::from_utf8_lossy(&bytes).into_owned())),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("an answer's status is {other}"),
        )),
    }
}

/// Answer every request on `stream` through `ports` until the process closes its end; what
/// ended the conversation otherwise, in words. Each call is the stage's own: its deadline, the
/// run's cancel, the waits the run counts (`StagePorts::call`).
pub fn serve<S: Read + Write>(stream: &mut S, ports: &StagePorts) -> Result<u64, String> {
    let mut calls = 0u64;
    loop {
        let (name, body) = match read_request(stream) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(calls),
            Err(e) => return Err(format!("the port protocol: {e}")),
        };
        calls += 1;
        let (status, bytes) = match ports.call(&name, body) {
            Ok(Answer::Ok(bytes)) => (ANSWERED, bytes),
            Ok(Answer::Error(reason)) => (REFUSED, reason.into_bytes()),
            Ok(Answer::Lost(reason)) => (
                REFUSED,
                format!("port `{name}` reached no answer: {reason}").into_bytes(),
            ),
            Err(failure) => (
                REFUSED,
                failure.into_error(ports.stage()).to_string().into_bytes(),
            ),
        };
        if let Err(e) = write_answer(stream, status, &bytes) {
            return Err(format!("answering a call to `{name}`: {e}"));
        }
    }
}

/// [`serve`] on a thread of its own, named after the task, for as long as the process holds its
/// end. What ended it other than the process closing its end is logged on the task's behalf.
pub fn serve_on_thread<S: Read + Write + Send + 'static>(
    mut stream: S,
    ports: Arc<StagePorts>,
    task: String,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("moruna-ports-{task}"))
        .spawn(move || {
            if let Err(why) = serve(&mut stream, &ports) {
                tracing::warn!(target: "sandbox", task = %task, "the task's ports stopped: {why}");
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;

    use moruna_kernel::{
        CancelToken, GuestCall, GuestPort, GuestRoute, Route, RunPorts, StageWaits,
    };

    struct Upper;

    impl GuestPort for Upper {
        fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
            if request.is_empty() {
                return Answer::Error("an empty request".into());
            }
            Answer::Ok(request.to_ascii_uppercase())
        }
    }

    fn ports() -> Arc<StagePorts> {
        let mut routes = BTreeMap::new();
        routes.insert(
            "warehouse.read".to_string(),
            Route {
                place: "guest",
                port: GuestRoute::new(Arc::new(Upper)),
                deadline: std::time::Duration::from_secs(5),
            },
        );
        let ports = StagePorts::new(1, routes);
        ports.bind_run(RunPorts {
            cancel: CancelToken::new(),
            waits: StageWaits::new(None),
        });
        ports
    }

    #[test]
    fn a_process_calls_its_ports_one_request_at_a_time() {
        let (mut task, runner) = UnixStream::pair().unwrap();
        let server = serve_on_thread(runner, ports(), "enrich".into()).unwrap();
        write_request(&mut task, "warehouse.read", b"orders").unwrap();
        assert_eq!(read_answer(&mut task).unwrap(), Ok(b"ORDERS".to_vec()));
        write_request(&mut task, "warehouse.read", b"").unwrap();
        assert_eq!(
            read_answer(&mut task).unwrap(),
            Err("an empty request".into())
        );
        write_request(&mut task, "warehouse.write", b"x").unwrap();
        let refused = read_answer(&mut task).unwrap().unwrap_err();
        assert!(
            refused.contains("`warehouse.write` is not listed"),
            "{refused}"
        );
        drop(task);
        server.join().unwrap();
    }

    #[test]
    fn what_is_not_a_request_ends_the_conversation() {
        let (mut task, mut runner) = UnixStream::pair().unwrap();
        task.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        drop(task);
        let why = serve(&mut runner, &ports()).unwrap_err();
        assert!(why.contains("does not speak the port protocol"), "{why}");
        let mut long = Vec::new();
        assert!(write_request(&mut long, &"x".repeat(65), b"").is_err());
        let mut half: &[u8] = b"MR";
        assert!(read_request(&mut half).is_err());
        let mut none: &[u8] = b"";
        assert!(read_request(&mut none).unwrap().is_none());
    }
}
