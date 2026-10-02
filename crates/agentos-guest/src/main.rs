//! `agentos-guest --fake UDS ROOT` runs the fake guest; the PID-1 and `exec-check` entry
//! points come with the VM backend.

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: agentos-guest --fake UDS ROOT";

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag, uds, root] if flag == "--fake" => match agentos_guest::fake::serve(Path::new(uds), Path::new(root)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agentos-guest: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
