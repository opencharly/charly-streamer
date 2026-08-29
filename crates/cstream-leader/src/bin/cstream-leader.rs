//! cstream-leader CLI — authenticate a user and report the outcome.
//!
//! The password is read from STDIN, never from argv: argv is world-readable via
//! /proc/<pid>/cmdline for the lifetime of the process, so a login leader taking
//! the credential as an argument leaks every password to any local account that
//! runs `ps`. Stdin is not observable that way.
//!
//! Exit codes are the contract the bed asserts:
//!   0  authenticated, account permitted, session opened
//!   1  authentication or account management refused
//!   2  usage / environment error
use std::io::Read;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: cstream-leader <service> <user>   # password on stdin");
        return ExitCode::from(2);
    }
    let mut password = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut password) {
        eprintln!("reading the password from stdin: {e}");
        return ExitCode::from(2);
    }
    // A trailing newline is what `echo pw |` and a heredoc both produce, and PAM
    // would compare it as part of the credential. Strip ONE line ending only: a
    // password may legitimately end with a space, and may legitimately end with a
    // newline of its own if the caller sent two.
    if password.ends_with('\n') {
        password.pop();
    }
    if password.ends_with('\r') {
        password.pop();
    }

    match cstream_leader::authenticate_and_open(&args[1], &args[2], &password) {
        Ok(_) => {
            println!("LEADER-AUTH-OK user={} service={}", args[2], args[1]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("LEADER-AUTH-FAIL {e}");
            ExitCode::from(1)
        }
    }
}
