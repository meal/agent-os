//! `agentos-guest`: with no arguments, the VM's PID 1 (`init::main`); `exec-check …`, the
//! check trampoline; `--fake UDS ROOT`, the fake guest for the host test tiers.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

/// The top-level usage; the `exec-check` line is the trampoline's own, so the two cannot drift.
fn usage() -> String {
    let exec_check = agentos_guest::trampoline::USAGE.trim_start_matches("usage: ");
    format!(
        "usage: agentos-guest --fake UDS ROOT\n       {exec_check}\n       agentos-guest   (as PID 1 in the guest VM only)"
    )
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        // Only the guest kernel starts us without arguments: PID 1 *and* its command line
        // naming us as init. Anywhere else (a shell, a container's PID 1) this would mkfs,
        // mount and reboot.
        [] if rustix::process::getpid().is_init()
            && agentos_guest::init::running_as_guest_init() =>
        {
            agentos_guest::init::main()
        }
        [cmd, rest @ ..] if cmd == "exec-check" => {
            let code = agentos_guest::trampoline::main(rest);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        [flag, uds, root] if flag == "--fake" => {
            match agentos_guest::fake::serve(Path::new(uds), Path::new(root)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("agentos-guest: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }
    }
}
