//! The fake guest: the agent's session over a Unix socket that speaks Firecracker's
//! host-side vsock handshake (`CONNECT 5200\n` / `OK 5200\n`), with `<root>/workspace` and
//! `<root>/scratch` standing in for the drives. Test-only; started as
//! `agentos-guest --fake UDS ROOT`.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use agentos_core::guest::VSOCK_PORT;

use crate::agent::{Exit, Session};
use crate::backend::FakeBackend;

/// The longest handshake line accepted (`CONNECT 5200\n` is 13 bytes).
const MAX_CONNECT_LINE: usize = 32;

/// A test hook: `name=1` together with `AGENTOS_TEST_WORKERS=1`.
pub(crate) fn test_hook(name: &str) -> bool {
    std::env::var_os("AGENTOS_TEST_WORKERS").is_some_and(|v| v == "1") && std::env::var_os(name).is_some_and(|v| v == "1")
}

/// Reads the handshake line byte by byte, so nothing of the first frame is consumed.
fn read_connect_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= MAX_CONNECT_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake line too long"));
        }
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    Ok(line)
}

fn handle(mut stream: UnixStream, mut backend: FakeBackend) {
    let expected = format!("CONNECT {VSOCK_PORT}\n");
    match read_connect_line(&mut stream) {
        Ok(line) if line == expected.as_bytes() => {}
        _ => return,
    }
    if stream.write_all(format!("OK {VSOCK_PORT}\n").as_bytes()).is_err() {
        return;
    }
    match Session::serve(&mut backend, stream, None) {
        // As a VM powers off: the whole guest goes away with its session.
        Exit::Shutdown | Exit::Lost => std::process::exit(0),
        Exit::Rejected => {}
    }
}

/// Listens on `uds` (a stale file is removed first) and serves every connection on its own
/// thread; the process exits 0 once a bound session ends. Returns only on a listener error,
/// or with `Ok` after the never-listen test hook.
pub fn serve(uds: &Path, root: &Path) -> io::Result<()> {
    if test_hook("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN") {
        thread::sleep(Duration::from_secs(30));
        return Ok(());
    }
    let backend = FakeBackend::new(root)?;
    match fs::remove_file(uds) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(uds)?;
    loop {
        let (stream, _) = listener.accept()?;
        let backend = backend.clone();
        thread::spawn(move || handle(stream, backend));
    }
}
