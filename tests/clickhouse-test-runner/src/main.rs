//! `clickhouse-test-runner` imitates `clickhouse-client` well enough for the
//! ClickHouse core test runner (`tests/clickhouse-test`) to drive it. It
//! plays the part of a thin binding of ch-core-rs (`INTEGRATING.md`): it owns
//! the transport, gets each result as `FORMAT Native`, decodes it with the
//! public API of the crate, and writes the decoded columns as `TabSeparated`
//! text, so the upstream `.reference` files check the decoder.
//!
//! See README.md for how to run the upstream SQL tests with it.

mod args;
mod errorcodes;
mod format;
mod hint;
mod http;
mod input;
#[cfg(test)]
mod live_tests;
mod log;
mod render;
mod runner;
mod split;
mod tcp;

use crate::args::{USAGE, parse_args, redact_password};
use crate::hint::error_name;
use crate::log::{InvocationLog, LOG_PATH_ENV};
use crate::runner::{Failure, PROTOCOL_ENV, Protocol, Runner};
use crate::split::split_statements;
use std::io::{Read, Write};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = run(
        &argv,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::exit(code);
}

pub(crate) fn run(
    argv: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    run_with_log(
        argv,
        std::env::var(LOG_PATH_ENV).ok(),
        std::env::var(PROTOCOL_ENV).ok().as_deref(),
        stdin,
        stdout,
        stderr,
    )
}

/// `run` with the path of the invocation log (`None`: no log) and the value
/// of the protocol selector. The tests give them here, because to set an
/// environment variable is not safe while other tests run.
pub(crate) fn run_with_log(
    argv: &[String],
    log_path: Option<String>,
    protocol: Option<&str>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if argv.first().map(String::as_str) == Some("extract-from-config") {
        return extract_from_config(&argv[1..], stdout);
    }

    let mut log = InvocationLog::new(log_path);
    log.add("argv", &redact_password(argv).join(" "));

    let opts = match parse_args(argv) {
        Ok(opts) => opts,
        Err(e) => {
            let _ = write!(stderr, "Error: {e}\n\n{USAGE}");
            return 1;
        }
    };
    if opts.help {
        return match stdout.write_all(USAGE.as_bytes()) {
            Ok(()) => 0,
            Err(_) => 1,
        };
    }

    let text = match &opts.query {
        Some(q) => q.clone(),
        None => {
            let mut buf = Vec::new();
            if let Err(e) = stdin.read_to_end(&mut buf) {
                let _ = writeln!(stderr, "Error: read stdin: {e}");
                return 1;
            }
            // Bytes that are not UTF-8 are kept (`src/input.rs`).
            match input::decode(buf) {
                Ok(text) => text,
                Err(e) => {
                    let _ = writeln!(stderr, "Error: {e}");
                    return 1;
                }
            }
        }
    };
    if text.trim().is_empty() {
        let _ = writeln!(
            stderr,
            "Error: no query. Use --query or write the queries to stdin."
        );
        return 1;
    }

    let stmts = split_statements(&text);
    log.add("statements", &stmts.len().to_string());
    if stmts.is_empty() {
        return 0;
    }

    let protocol = match Protocol::from_env(protocol) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(stderr, "Error: {e}");
            return 1;
        }
    };
    log.add("protocol", protocol.name());
    let result = Runner::new(&opts, protocol, &mut log).and_then(|mut r| r.run(&stmts, stdout));
    match result {
        Ok(()) => 0,
        Err(failure) => {
            log.add("error", &format!("{failure:?}"));
            report_error(stderr, &failure)
        }
    }
}

/// Writes the failure to stderr and returns the exit code. As in
/// `clickhouse-client`, the exit code is the error code modulo 256 when there
/// is an error code.
fn report_error(stderr: &mut dyn Write, failure: &Failure) -> i32 {
    // The messages can quote a statement: write its original bytes.
    let mut write = |text: String| {
        let _ = stderr.write_all(&input::encode(&text));
    };
    let code = match failure {
        Failure::Hint { message, code } => {
            write(format!("Error: {message}\n"));
            *code
        }
        Failure::Server { code, message } => {
            let mut message = message.clone();
            if let Some(name) = error_name(*code)
                && !message.contains(&format!("({name})"))
            {
                message.push_str(&format!(" ({name})"));
            }
            write(format!("Received exception from server:\n{message}\n"));
            Some(*code)
        }
        Failure::Other(message) => {
            write(format!("Error: {message}\n"));
            None
        }
    };
    match code.map(|c| c.rem_euclid(256)) {
        Some(c) if c != 0 => c,
        _ => 1,
    }
}

/// Answers `clickhouse extract-from-config --key <key>`, which
/// `tests/clickhouse-test` runs to read the server config. Only `listen_host`
/// has an answer; for other keys the output is empty, as for a key that is not
/// in the config.
fn extract_from_config(argv: &[String], stdout: &mut dyn Write) -> i32 {
    let mut key = None;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--key" && i + 1 < argv.len() {
            i += 1;
            key = Some(argv[i].as_str());
        } else if let Some(k) = a.strip_prefix("--key=") {
            key = Some(k);
        }
        i += 1;
    }
    if key == Some("listen_host") && writeln!(stdout, "127.0.0.1").is_err() {
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_listen_host() {
        let mut out = Vec::new();
        let argv = strings(&[
            "extract-from-config",
            "--try",
            "--config",
            "x.xml",
            "--key",
            "listen_host",
        ]);
        assert_eq!(
            run(&argv, &mut std::io::empty(), &mut out, &mut Vec::new()),
            0
        );
        assert_eq!(out, b"127.0.0.1\n");

        out.clear();
        let argv = strings(&["extract-from-config", "--key=tcp_port"]);
        assert_eq!(
            run(&argv, &mut std::io::empty(), &mut out, &mut Vec::new()),
            0
        );
        assert!(out.is_empty());
    }

    #[test]
    fn exit_codes() {
        let mut err = Vec::new();
        let server = Failure::Server {
            code: 60,
            message: "Code: 60. DB::Exception: Unknown table".into(),
        };
        assert_eq!(report_error(&mut err, &server), 60);
        assert!(String::from_utf8_lossy(&err).contains("(UNKNOWN_TABLE)"));
        let wrapped = Failure::Server {
            code: 512,
            message: "Code: 512.".into(),
        };
        assert_eq!(report_error(&mut Vec::new(), &wrapped), 1);
        assert_eq!(
            report_error(&mut Vec::new(), &Failure::Other("x".into())),
            1
        );
    }

    #[test]
    fn no_query_is_an_error() {
        let mut err = Vec::new();
        assert_eq!(
            run(&[], &mut "  \n".as_bytes(), &mut Vec::new(), &mut err),
            1
        );
        assert!(!err.is_empty());
    }

    #[test]
    fn comment_only_input_runs_nothing() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&[], &mut "-- nothing\n".as_bytes(), &mut out, &mut err),
            0
        );
        assert!(out.is_empty() && err.is_empty());
    }
}
