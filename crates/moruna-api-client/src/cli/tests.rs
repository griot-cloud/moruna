//! The command's arguments and its errors, without a node. Its answers from a node are tested in
//! `tests/clients.rs`.

use super::*;

fn args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// The command's exit code, stdout and stderr.
fn run_node(given: &[&str]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = node(&args(given), &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn every_option_is_read() {
    let options = parse(&args(&[
        "--url",
        "https://n",
        "--token",
        "t",
        "--pin",
        "p",
        "--audit",
    ]))
    .unwrap();
    assert_eq!(
        options,
        Options {
            url: Some("https://n".into()),
            token: Some("t".into()),
            pin: Some("p".into()),
            audit: true,
            help: false,
        }
    );
}

#[test]
fn a_bad_argument_is_refused_by_name() {
    let cases: [(&[&str], &str); 4] = [
        (&["--url", "a", "--url", "b"], "--url is given once"),
        (&["--token"], "--token needs a value"),
        (&["--pin", "--audit"], "--pin needs a value"),
        (&["node"], "unknown argument \"node\""),
    ];
    for (given, said) in cases {
        assert_eq!(parse(&args(given)).unwrap_err(), said);
    }
}

#[test]
fn help_prints_the_usage_and_succeeds() {
    for flag in ["--help", "-h"] {
        let (code, out, err) = run_node(&[flag]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, format!("{USAGE}\n"));
    }
}

#[test]
fn a_bad_argument_exits_1_with_the_usage_on_stderr() {
    let (code, out, err) = run_node(&["--url"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(err, format!("moruna node: --url needs a value\n{USAGE}\n"));
}

#[test]
fn the_url_and_the_token_are_both_needed() {
    for given in [&["--url", "https://n"][..], &["--token", "t"], &[]] {
        let (code, out, err) = run_node(given);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.starts_with("moruna node: --url and --token are both needed\nusage:"),
            "{err}"
        );
    }
}

#[test]
fn a_client_that_cannot_be_made_is_an_error() {
    let (code, out, err) = run_node(&["--url", "http://203.0.113.7", "--token", "t"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        "moruna node: the URL \"http://203.0.113.7\" cannot be called: a node's API is served \
         over HTTPS only\n"
    );
}

#[test]
fn an_unreachable_node_is_an_error() {
    // Port 1 on the loopback: nothing listens there, so the connection is refused at once.
    let pin = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    for audit in [false, true] {
        let mut given = vec!["--url", "https://127.0.0.1:1", "--token", "t", "--pin", pin];
        if audit {
            given.push("--audit");
        }
        let (code, out, err) = run_node(&given);
        assert_eq!((code, out.as_str()), (1, ""));
        let operation = if audit { "node_audit" } else { "node" };
        assert!(
            err.starts_with(&format!(
                "moruna node: could not call {operation} at https://127.0.0.1:1/"
            )),
            "{err}"
        );
    }
}

/// A writer that refuses every write, as a closed stdout does.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_closed_stdout_is_an_error() {
    let mut err = Vec::new();
    assert_eq!(node(&args(&["--help"]), &mut Closed, &mut err), 1);
    assert!(String::from_utf8(err).unwrap().starts_with("moruna node: "));
}
