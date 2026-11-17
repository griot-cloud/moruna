//! `moruna node` (system design 2E): read a node's document, or its audit, over its API.
//!
//! ```text
//! moruna node --url <https://...> --token <token> [--pin <hex>] [--audit]
//! ```
//!
//! The answer is printed as JSON on stdout. An error goes to stderr, and the command exits 1. The
//! `moruna` binary (`crates/moruna-py/src/bin/moruna.rs`) runs this before it starts its
//! interpreter, which the command does not need.

use std::io::Write;

use crate::Client;

/// The command's usage.
pub const USAGE: &str =
    "usage: moruna node --url <https://...> --token <token> [--pin <hex>] [--audit]

Print a node's document, or with --audit the most recent calls to it, as JSON.

  --url <https://...>  the node's API address
  --token <token>      a token for the node: observe scope for the document, admin for --audit
  --pin <hex>          accept the node's certificate exactly when its SHA-256 is this, the
                       fingerprint the node prints on its console (moruna: tls sha256=<hex>);
                       without it, the certificate is verified against the system's roots
  --audit              print the most recent calls instead of the document";

/// Run `moruna node` with the arguments after `node`, writing the answer to `out` and an error to
/// `err`. Returns the exit code: 0, or 1 on any error.
pub fn node(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(()) => 0,
        Err(message) => {
            // Nothing more can be said when stderr itself cannot be written.
            let _ = writeln!(err, "moruna node: {message}");
            1
        }
    }
}

/// The command's options.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    url: Option<String>,
    token: Option<String>,
    pin: Option<String>,
    audit: bool,
    help: bool,
}

fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let options = parse(args).map_err(|message| format!("{message}\n{USAGE}"))?;
    if options.help {
        return writeln!(out, "{USAGE}").map_err(|error| error.to_string());
    }
    let (Some(url), Some(token)) = (options.url, options.token) else {
        return Err(format!("--url and --token are both needed\n{USAGE}"));
    };
    let client = Client::new(&url, token, options.pin.as_deref()).map_err(|e| e.to_string())?;
    let json = if options.audit {
        client
            .node_audit()
            .map(|audit| serde_json::to_string_pretty(&audit))
    } else {
        client
            .node()
            .map(|node| serde_json::to_string_pretty(&node))
    }
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    writeln!(out, "{json}").map_err(|error| error.to_string())
}

/// Read the options: each valued option once, with its value as the next argument.
fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--url" => &mut options.url,
            "--token" => &mut options.token,
            "--pin" => &mut options.pin,
            "--audit" => {
                options.audit = true;
                continue;
            }
            "--help" | "-h" => {
                options.help = true;
                continue;
            }
            other => return Err(format!("unknown argument {other:?}")),
        };
        if slot.is_some() {
            return Err(format!("{arg} is given once"));
        }
        match args.next() {
            Some(value) if !value.starts_with("--") => *slot = Some(value.clone()),
            _ => return Err(format!("{arg} needs a value")),
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests;
