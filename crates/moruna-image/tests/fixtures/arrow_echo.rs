//! The program the image kind's tests run as an image's command (SDD 19 k): it reads an Arrow IPC
//! stream on standard input and writes one back on standard output, each batch with a column `n`
//! added (the row's index in the batch), which is the whole contract of an image (SDD 19 f.6).
//!
//! One optional argument makes it misbehave, each in the way a test needs:
//!
//! - `--socket` opens a TCP connection before anything else;
//! - `--write-root` tries to create `/etc/x` after proving `/tmp` is writable;
//! - `--garbage` writes bytes that are no Arrow stream;
//! - `--exit 3` does its work and then exits with status 3 where it should have ended the stream;
//! - `--report` says on standard error who and where it is (its user, its process id, its
//!   environment, the entries of `/` and of `/proc`) and exits with status 9.

use std::io::{Read, Write};
use std::process::exit;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

fn die(what: &str, e: impl std::fmt::Display) -> ! {
    eprintln!("arrow_echo: {what}: {e}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    if flag("--socket") {
        match std::net::TcpStream::connect(("127.0.0.1", 9)) {
            Ok(_) => die("a connection", "it was made"),
            Err(e) => die("the connection", e),
        }
    }
    if flag("--report") {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let uid = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .unwrap_or("Uid: ?");
        eprintln!("report: {uid}");
        eprintln!("report: pid {}", std::process::id());
        let mut env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
        env.sort();
        eprintln!("report: env {}", env.join(" "));
        let list = |dir: &str| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names.join(",")
        };
        eprintln!("report: root {}", list("/"));
        eprintln!("report: dev {}", list("/dev"));
        let procs: Vec<String> = list("/proc")
            .split(',')
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty())
            .map(String::from)
            .collect();
        eprintln!("report: processes {}", procs.join(","));
        exit(9);
    }
    if flag("--write-root") {
        if let Err(e) = std::fs::write("/tmp/arrow_echo", b"ok") {
            die("writing /tmp/arrow_echo", e);
        }
        match std::fs::File::create("/etc/x") {
            Ok(_) => eprintln!("arrow_echo: /etc/x was created"),
            Err(e) => die("creating /etc/x", e),
        }
    }
    if flag("--garbage") {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"this is not an Arrow stream, and it says so\n");
        let _ = out.flush();
        // Drain the input so the parent's writes never block on a child that has stopped
        // reading, then end well.
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        return;
    }
    let exit_with = args.iter().position(|a| a == "--exit").map(|at| {
        match args.get(at + 1).and_then(|code| code.parse::<i32>().ok()) {
            Some(code) => code,
            None => die("--exit", "it takes a status"),
        }
    });

    let mut input = StreamReader::try_new(std::io::stdin().lock(), None)
        .unwrap_or_else(|e| die("reading the input schema", e));
    let in_schema = input.schema();
    let mut fields: Vec<Arc<Field>> = in_schema.fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new("n", DataType::Int64, false)));
    let out_schema = Arc::new(Schema::new(fields));
    let mut output = StreamWriter::try_new(std::io::stdout().lock(), &out_schema)
        .unwrap_or_else(|e| die("writing the output schema", e));
    let _ = output.get_mut().flush();
    for batch in input.by_ref() {
        let batch = batch.unwrap_or_else(|e| die("reading a batch", e));
        let rows = batch.num_rows() as i64;
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        columns.push(Arc::new(Int64Array::from_iter_values(0..rows)));
        let echoed = RecordBatch::try_new(Arc::clone(&out_schema), columns)
            .unwrap_or_else(|e| die("building a batch", e));
        output
            .write(&echoed)
            .unwrap_or_else(|e| die("writing a batch", e));
        output
            .get_mut()
            .flush()
            .unwrap_or_else(|e| die("flushing a batch", e));
    }
    // Standard input ended: that is the end of the stream (SDD 19 f.6).
    drop(input);
    if let Some(code) = exit_with {
        exit(code);
    }
    output
        .finish()
        .unwrap_or_else(|e| die("ending the output", e));
    let _ = output.get_mut().flush();
    let mut rest = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut rest);
}
