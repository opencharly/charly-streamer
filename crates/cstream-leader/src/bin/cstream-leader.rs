//! cstream-leader CLI — authenticate a user and report the outcome.
//!
//! Exit codes are the contract the bed asserts:
//!   0  authenticated, account permitted, session opened
//!   1  authentication or account management refused
//!   2  usage / environment error
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: cstream-leader <service> <user> <password>");
        return ExitCode::from(2);
    }
    match cstream_leader::authenticate_and_open(&args[1], &args[2], &args[3]) {
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
