//! `agentos-guest`: with no arguments, the VM's PID 1 (`init::main`); `exec-check …`, the
//! check trampoline; `--fake UDS ROOT`, the fake guest for the host test tiers.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: agentos-guest --fake UDS ROOT\n       \
                     agentos-guest exec-check --nproc N --nofile N --oom N -- PROGRAM [ARGS...]\n       \
                     agentos-guest   (as PID 1 in the guest VM only)";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        // Only the kernel starts us without arguments, as PID 1; anywhere else this would
        // mount over the host's /proc and reboot it.
        [] if rustix::process::getpid().is_init() => agentos_guest::init::main(),
        [cmd, rest @ ..] if cmd == "exec-check" => {
            let code = agentos_guest::trampoline::main(rest);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
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
