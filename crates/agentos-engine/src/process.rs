//! Running a check in its own process group, so nothing it starts outlives it.

use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::job::append_group;

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
    let _ = (&mut pipe)
        .take(limit as u64 + 1)
        .read_to_end(&mut kept)
        .await;
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
///
/// With `groups_file`, the group id is appended to it (durably) before the wait, so a
/// supervisor that outlives this process can still kill the group. If it cannot be
/// recorded, the group is killed at once and nothing runs unrecorded.
///
/// Known window: between `spawn` and the fsynced record the group exists unrecorded. A
/// worker SIGKILLed inside that window leaves the check's group running where the
/// supervisor cannot see it. Closing it needs a pre-exec gate (the child waits until the
/// parent has recorded it), left to a later task.
pub(crate) async fn run_in_group(
    mut cmd: Command,
    timeout: Duration,
    limit: usize,
    groups_file: Option<&Path>,
) -> Result<GroupOutput, GroupError> {
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(GroupError::Io)?;
    let pgid = child.id().and_then(|id| Pid::from_raw(id as i32));
    if let Some(file) = groups_file {
        let recorded = match pgid {
            Some(p) => append_group(file, p.as_raw_nonzero().get()),
            None => Err(io::Error::other("spawned child has no pid")),
        };
        if let Err(e) = recorded {
            kill_group(pgid);
            let _ = child.kill().await;
            return Err(GroupError::Io(io::Error::new(
                e.kind(),
                format!("cannot record process group: {e}"),
            )));
        }
    }
    let stdout = tokio::spawn(capture(
        child.stdout.take().expect("stdout is piped"),
        limit,
    ));
    let stderr = tokio::spawn(capture(
        child.stderr.take().expect("stderr is piped"),
        limit,
    ));

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
    let join = |r: Result<Vec<u8>, tokio::task::JoinError>| {
        r.map_err(|e| GroupError::Io(io::Error::other(e)))
    };
    Ok(GroupOutput {
        status,
        stdout: join(stdout.await)?,
        stderr: join(stderr.await)?,
    })
}
