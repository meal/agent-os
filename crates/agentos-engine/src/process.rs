//! Running a check in its own process group, so nothing it starts outlives it.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{kill_process_group, Pid, Signal};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

pub(crate) struct GroupOutput {
    pub status: ExitStatus,
    /// At most `limit + 1` bytes, so callers can tell the output was cut.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum GroupError {
    Timeout,
    Io(io::Error),
}

/// Keeps the first `limit + 1` bytes and drains the rest, so a chatty child never blocks on
/// a full pipe.
async fn capture(mut pipe: impl AsyncRead + Unpin, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut pipe).take(limit as u64 + 1).read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
    kept
}

/// SIGKILLs every process in group `pgid`. ESRCH (group already gone) is fine.
fn kill_group(pgid: Option<Pid>) {
    if let Some(pgid) = pgid {
        let _ = kill_process_group(pgid, Signal::KILL);
    }
}

/// Runs `cmd` as the leader of a new process group and, once the leader exits or the
/// timeout fires, kills the whole group: background grandchildren never outlive the run.
///
/// The group id is the leader's pid, and no pid can be reused while a group with that id
/// has members, so the kill reaches only processes this run started. (If the group is
/// already empty, the kill could only misfire if the reaped pid became a new group leader
/// in the instant between reaping and killing.)
/// A descendant that calls `setsid` leaves the group and escapes this; containing that is
/// the job of the VM/Wasm backends.
pub(crate) async fn run_in_group(mut cmd: Command, timeout: Duration, limit: usize) -> Result<GroupOutput, GroupError> {
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(GroupError::Io)?;
    let pgid = child.id().and_then(|id| Pid::from_raw(id as i32));
    let stdout = tokio::spawn(capture(child.stdout.take().expect("stdout is piped"), limit));
    let stderr = tokio::spawn(capture(child.stderr.take().expect("stderr is piped"), limit));

    let waited = tokio::time::timeout(timeout, child.wait()).await;
    // Kill the group before reaping matters: survivors would otherwise keep the pipes open.
    kill_group(pgid);
    let status = match waited {
        Err(_) => {
            let _ = child.kill().await;
            return Err(GroupError::Timeout);
        }
        Ok(status) => status.map_err(GroupError::Io)?,
    };
    let join = |r: Result<Vec<u8>, tokio::task::JoinError>| r.map_err(|e| GroupError::Io(io::Error::other(e)));
    Ok(GroupOutput { status, stdout: join(stdout.await)?, stderr: join(stderr.await)? })
}
