//! `RunAgent` (protocol 2): one run of an agent CLI in a scratch copy of the workspace, with
//! its model calls relayed to the host. The real workspace is never written. The reply is the
//! patch the run made against the baseline it started from, plus the digest of that tree.
//!
//! Ownership: the session loop (`supervise`) owns the stream. The model proxy's connection
//! threads reach it only through an mpsc channel (`Event::Request`), one request in flight at
//! a time (the bridge's gate). A waiter thread reports the child's exit on the same channel,
//! so the loop wakes for exits, requests and the deadline. The one blocking spot is the read
//! of a `ModelReply`: the loop waits for the host there, and the host's lease is the backstop
//! for a host that never answers.
//!
//! The scratch repository's git dir lives outside the tree and is owned by the builder uid,
//! so the CLI cannot reach its configuration. Ownership of the tree alternates: the builder
//! (root copies, then `git` runs) and the agent (while the CLI runs).

use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentos_core::guest::{
    Frame, Message, OUTPUT_LIMIT, PATCH_LIMIT, RAW_FRAME_LIMIT, read_frame, write_frame,
};
use agentos_core::ids::Digest;
use agentos_core::patchrules::{check_summary, parse_numstat};
use agentos_core::workspace::{copy_tree, purge_excluded, workspace_digest};
use rustix::process::Pid;

use crate::backend::{Backend, CHECK_PATH};
use crate::handlers::{WORKSPACE_MISSING, capture, fresh_scratch, kill_group, reap_group};
use crate::proxy::{Bridge, Proxy};

/// Directories the agent may leave in its tree that are not part of the patch, on top of the
/// workspace's excluded entries (`agentos_core::workspace`). They are removed before the
/// baseline is taken and again before the patch is cut, so they never show as changes.
const SESSION_EXCLUDED: [&str; 2] = [".pytest_cache", ".claude"];

/// Variables the session sets itself: a message cannot override them.
const SESSION_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "PYTHONDONTWRITEBYTECODE",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "DISABLE_AUTOUPDATER",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_TELEMETRY",
];

/// Why a RunAgent did not finish. `Refused` is answered and the session goes on; `Lost` ends
/// the session without a reply, because the stream can no longer be trusted.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentError {
    Refused(String),
    Lost(String),
}

/// What a finished run answers with: `AgentDone`, and the raw frame that follows it.
#[derive(Debug, PartialEq, Eq)]
pub struct Finished {
    pub message: Message,
    pub patch: Vec<u8>,
}

/// What the loop saw of the child.
struct Ended {
    status: ExitStatus,
    timed_out: bool,
}

enum Event {
    /// A proxy connection's model call: the body to send, and where its reply goes.
    Request {
        body: Vec<u8>,
        reply: mpsc::Sender<Result<(u16, Vec<u8>), String>>,
    },
    /// The child has exited (and been reaped by the waiter).
    Exited,
}

/// The proxy's bridge: one `ModelRequest` in flight, answered by the session loop.
struct SessionBridge {
    events: mpsc::Sender<Event>,
    gate: Mutex<()>,
}

impl Bridge for SessionBridge {
    fn forward(&self, body: Vec<u8>) -> Result<(u16, Vec<u8>), String> {
        let _one = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let (reply, answer) = mpsc::channel();
        self.events
            .send(Event::Request { body, reply })
            .map_err(|_| "the agent session has ended".to_string())?;
        answer
            .recv()
            .map_err(|_| "the agent session ended before the model replied".to_string())?
    }
}

/// The child and its group. Dropping it kills the group and reaps it; `stop` does the same and
/// returns the child's status.
struct Running {
    pgid: Option<Pid>,
    waiter: Option<JoinHandle<io::Result<ExitStatus>>>,
}

impl Running {
    fn stop(&mut self) -> io::Result<ExitStatus> {
        kill_group(self.pgid);
        let status = match self.waiter.take() {
            Some(waiter) => waiter
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("waiter thread panicked"))),
            None => Err(io::Error::other("the child was already stopped")),
        };
        reap_group(self.pgid);
        status
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.waiter.is_some() {
            let _ = self.stop();
        }
    }
}

/// The directories of one run, under `<scratch>/agent`.
struct Tree {
    work: PathBuf,
    gitdir: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
}

impl Tree {
    fn repo<'a>(&self, backend: &'a dyn Backend) -> Repo<'a> {
        Repo {
            backend,
            work: self.work.clone(),
            gitdir: self.gitdir.clone(),
        }
    }
}

/// `git` on the agent's repository, with the gitdir kept outside the tree.
struct Repo<'a> {
    backend: &'a dyn Backend,
    work: PathBuf,
    gitdir: PathBuf,
}

impl Repo<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.backend.git(&self.work);
        cmd.arg(format!("--git-dir={}", self.gitdir.display()))
            .arg(format!("--work-tree={}", self.work.display()))
            .args(args)
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let out = self
            .command(args)
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// The stdout of a read-only `git` call, or `None` if it is longer than `limit`.
    fn stdout(&self, args: &[&str], limit: usize) -> Result<Option<Vec<u8>>, String> {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        let stderr = capture(child.stderr.take().expect("stderr is piped"), 4096);
        let mut out = Vec::new();
        let read = child
            .stdout
            .take()
            .expect("stdout is piped")
            .take(limit as u64 + 1)
            .read_to_end(&mut out);
        let over = out.len() > limit;
        if over {
            let _ = child.kill();
        }
        let status = child
            .wait()
            .map_err(|e| format!("cannot wait for git: {e}"))?;
        let stderr = stderr.join().unwrap_or_default();
        read.map_err(|e| format!("cannot read git output: {e}"))?;
        if over {
            return Ok(None);
        }
        if !status.success() {
            return Err(format!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        Ok(Some(out))
    }
}

/// Removes the workspace's excluded entries and the session's own, never following symlinks.
fn purge_leftovers(dir: &Path) -> io::Result<()> {
    purge_excluded(dir)?;
    purge_named(dir)
}

fn purge_named(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let name = entry.file_name();
        if SESSION_EXCLUDED
            .iter()
            .any(|n| name == std::ffi::OsStr::new(n))
        {
            if meta.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        } else if meta.is_dir() {
            purge_named(&path)?;
        }
    }
    Ok(())
}

/// The scratch copy, its baseline commit and the agent's directories. The reply's repository
/// steps run as the builder (`backend.git`); the CLI gets the tree only.
fn prepare_tree(backend: &dyn Backend, ws: &Path) -> Result<Tree, AgentError> {
    let refuse = AgentError::Refused;
    let root = fresh_scratch(backend, "agent").map_err(refuse)?;
    let tree = Tree {
        work: root.join("work"),
        gitdir: root.join("git"),
        home: root.join("home"),
        tmp: root.join("tmp"),
    };
    copy_tree(ws, &tree.work).map_err(|e| refuse(format!("cannot copy the workspace: {e}")))?;
    for dir in [&tree.gitdir, &tree.home, &tree.tmp] {
        fs::create_dir_all(dir).map_err(|e| refuse(format!("scratch dir: {e}")))?;
    }
    purge_leftovers(&tree.work).map_err(|e| refuse(format!("cannot clean the copy: {e}")))?;
    backend.own_tree(&tree.work).map_err(refuse)?;
    backend.own_tree(&tree.gitdir).map_err(refuse)?;
    let repo = tree.repo(backend);
    repo.run(&["init", "-q"]).map_err(refuse)?;
    repo.run(&["config", "user.name", "agentos"])
        .map_err(refuse)?;
    repo.run(&["config", "user.email", "agentos@localhost"])
        .map_err(refuse)?;
    repo.run(&["config", "core.autocrlf", "false"])
        .map_err(refuse)?;
    // `-f`: a `.gitignore` in the workspace must not hide files from the baseline or the patch.
    repo.run(&["add", "-A", "-f", "."]).map_err(refuse)?;
    repo.run(&[
        "commit",
        "-q",
        "--allow-empty",
        "--no-verify",
        "-m",
        "baseline",
    ])
    .map_err(refuse)?;
    for dir in [&tree.work, &tree.home, &tree.tmp] {
        backend.hand_to_agent(dir).map_err(refuse)?;
    }
    Ok(tree)
}

/// The child's environment: exactly the session's variables plus the message's, minus any
/// name the session sets itself. The proxy's address is the only network endpoint offered.
fn env_for_cli(
    tree: &Tree,
    proxy: SocketAddr,
    requested: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_string(), CHECK_PATH.to_string()),
        ("HOME".to_string(), tree.home.display().to_string()),
        ("TMPDIR".to_string(), tree.tmp.display().to_string()),
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        ("ANTHROPIC_BASE_URL".to_string(), format!("http://{proxy}")),
        ("ANTHROPIC_API_KEY".to_string(), "placeholder".to_string()),
        ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
            "1".to_string(),
        ),
        ("DISABLE_TELEMETRY".to_string(), "1".to_string()),
    ];
    env.extend(
        requested
            .iter()
            .filter(|(k, _)| !SESSION_ENV.contains(&k.as_str()))
            .cloned(),
    );
    env
}

fn check_env(env: &[(String, String)]) -> Result<(), String> {
    for (k, v) in env {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            return Err(format!("invalid environment entry {k:?}"));
        }
    }
    Ok(())
}

/// One model call on the stream: `ModelRequest { id }` and its body, then the `ModelReply` and
/// its body. Any other frame is a protocol failure.
fn relay<S: Read + Write>(
    stream: &mut S,
    id: u64,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), String> {
    write_frame(stream, &Frame::Json(Message::ModelRequest { id }))
        .map_err(|e| format!("cannot send ModelRequest: {e}"))?;
    write_frame(stream, &Frame::Raw(body)).map_err(|e| format!("cannot send the body: {e}"))?;
    let status = match read_frame(stream, RAW_FRAME_LIMIT).map_err(|e| e.to_string())? {
        Frame::Json(Message::ModelReply { id: got, status }) if got == id => status,
        _ => return Err(format!("expected ModelReply {id}")),
    };
    match read_frame(stream, RAW_FRAME_LIMIT).map_err(|e| e.to_string())? {
        Frame::Raw(reply) => Ok((status, reply)),
        Frame::Json(_) => Err("expected the body of ModelReply".into()),
    }
}

/// Runs the child until it exits or its deadline passes, relaying model calls in between.
/// Owns `events`, so a request left unanswered is dropped (and its proxy connection fails)
/// when this returns.
#[allow(clippy::too_many_arguments)]
fn supervise<S: Read + Write>(
    backend: &dyn Backend,
    stream: &mut S,
    tree: &Tree,
    argv: &[String],
    env: &[(String, String)],
    timeout_secs: u64,
    proxy: SocketAddr,
    events_tx: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
) -> Result<Ended, AgentError> {
    let mut cmd = backend.agent_command(argv, &tree.work);
    cmd.env_clear()
        .envs(env_for_cli(tree, proxy, env))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| AgentError::Refused(format!("cannot run {}: {e}", argv[0])))?;
    // Output is bounded and dropped: none of it reaches the host yet.
    drop(capture(
        child.stdout.take().expect("stdout is piped"),
        OUTPUT_LIMIT,
    ));
    drop(capture(
        child.stderr.take().expect("stderr is piped"),
        OUTPUT_LIMIT,
    ));
    let pgid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let waiter = thread::spawn(move || {
        let status = child.wait();
        let _ = events_tx.send(Event::Exited);
        status
    });
    let mut running = Running {
        pgid,
        waiter: Some(waiter),
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut next_id = 1u64;
    let mut timed_out = false;
    loop {
        // Checked on every turn: a steady stream of model calls must not starve the deadline.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            timed_out = true;
            break;
        }
        match events.recv_timeout(left) {
            Ok(Event::Request { body, reply }) => {
                let id = next_id;
                next_id += 1;
                match relay(stream, id, body) {
                    Ok(answer) => {
                        let _ = reply.send(Ok(answer));
                    }
                    Err(why) => {
                        let _ = reply.send(Err(why.clone()));
                        return Err(AgentError::Lost(why));
                    }
                }
            }
            Ok(Event::Exited) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AgentError::Lost(
                    "the agent session lost its event source".into(),
                ));
            }
        }
    }
    let status = running
        .stop()
        .map_err(|e| AgentError::Refused(format!("cannot wait for the agent: {e}")))?;
    Ok(Ended { status, timed_out })
}

/// The model proxy and the child, both gone when this returns.
fn run_child<S: Read + Write>(
    backend: &dyn Backend,
    stream: &mut S,
    tree: &Tree,
    argv: &[String],
    env: &[(String, String)],
    timeout_secs: u64,
) -> Result<Ended, AgentError> {
    let (events_tx, events) = mpsc::channel();
    let bridge = Arc::new(SessionBridge {
        events: events_tx.clone(),
        gate: Mutex::new(()),
    });
    let proxy = Proxy::start(bridge)
        .map_err(|e| AgentError::Refused(format!("cannot start the model proxy: {e}")))?;
    let ended = supervise(
        backend,
        stream,
        tree,
        argv,
        env,
        timeout_secs,
        proxy.addr,
        events_tx,
        events,
    );
    // Stopped only after `events` is gone: no connection is left waiting for an answer.
    proxy.stop();
    ended
}

/// The patch the run made, and the digest of the tree it was cut from. Repository steps run
/// as the builder, after the agent's uid has let go of the tree.
fn cut_patch(backend: &dyn Backend, tree: &Tree) -> Result<(Vec<u8>, Digest), AgentError> {
    let refuse = AgentError::Refused;
    purge_leftovers(&tree.work)
        .map_err(|e| refuse(format!("cannot clean the agent's tree: {e}")))?;
    let digest = workspace_digest(&tree.work)
        .map_err(|e| refuse(format!("cannot digest the agent's tree: {e}")))?;
    backend.own_tree(&tree.work).map_err(refuse)?;
    let repo = tree.repo(backend);
    repo.run(&["add", "-A", "-f", "."]).map_err(refuse)?;
    let too_long = || refuse(format!("the change is over {PATCH_LIMIT} bytes"));
    let summary = repo
        .stdout(
            &[
                "diff",
                "--cached",
                "--no-renames",
                "--no-color",
                "--summary",
                "HEAD",
            ],
            PATCH_LIMIT,
        )
        .map_err(refuse)?
        .ok_or_else(too_long)?;
    check_summary(&summary).map_err(refuse)?;
    let numstat = repo
        .stdout(
            &[
                "diff",
                "--cached",
                "--no-renames",
                "--no-color",
                "--numstat",
                "-z",
                "HEAD",
            ],
            PATCH_LIMIT,
        )
        .map_err(refuse)?
        .ok_or_else(too_long)?;
    if numstat.is_empty() {
        return Ok((Vec::new(), digest));
    }
    parse_numstat(&numstat).map_err(refuse)?;
    let patch = repo
        .stdout(
            &[
                "diff",
                "--cached",
                "--no-renames",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
            ],
            PATCH_LIMIT,
        )
        .map_err(refuse)?
        .ok_or_else(too_long)?;
    Ok((patch, digest))
}

/// `RunAgent`: validates the request, runs the CLI in a scratch copy with `lo` up for the
/// proxy's duration, and answers with the patch. The real workspace is only read.
pub fn run_agent<S: Read + Write>(
    backend: &dyn Backend,
    stream: &mut S,
    argv: &[String],
    env: &[(String, String)],
    timeout_secs: u64,
    expected_base: Digest,
) -> Result<Finished, AgentError> {
    let refuse = AgentError::Refused;
    if !backend.workspace_present() {
        return Err(refuse(WORKSPACE_MISSING.into()));
    }
    let Some(program) = argv.first() else {
        return Err(refuse("argv is empty".into()));
    };
    if !Path::new(program).is_absolute() {
        return Err(refuse(format!(
            "argv[0] must be an absolute path: {program}"
        )));
    }
    if timeout_secs == 0 {
        return Err(refuse("timeout_secs must be at least 1".into()));
    }
    check_env(env).map_err(refuse)?;
    let ws = backend.workspace_dir().to_path_buf();
    let actual =
        workspace_digest(&ws).map_err(|e| refuse(format!("cannot digest workspace: {e}")))?;
    if actual != expected_base {
        return Err(refuse(format!(
            "version conflict: expected {expected_base}, actual {actual}"
        )));
    }
    let tree = prepare_tree(backend, &ws)?;
    backend
        .check_program(program, &tree.work)
        .map_err(|e| refuse(format!("cannot run {program}: {e}")))?;

    backend
        .set_loopback(true)
        .map_err(|e| refuse(format!("cannot bring lo up: {e}")))?;
    let ran = run_child(backend, stream, &tree, argv, env, timeout_secs);
    // `lo` goes down whatever happened: loopback is reachable only while a session needs it.
    let down = backend.set_loopback(false);
    let ended = ran?;
    down.map_err(|e| AgentError::Lost(format!("cannot take lo down: {e}")))?;

    let (patch, tree_digest) = cut_patch(backend, &tree)?;
    // The real workspace is what the run started from: nothing wrote to it.
    let now = workspace_digest(&ws).map_err(|e| refuse(format!("cannot digest workspace: {e}")))?;
    if now != expected_base {
        return Err(refuse(format!(
            "workspace changed during the agent session: expected {expected_base}, actual {now}"
        )));
    }
    Ok(Finished {
        message: Message::AgentDone {
            exit_code: ended.status.code(),
            signal: ended.status.signal(),
            timed_out: ended.timed_out,
            workspace_digest: tree_digest,
        },
        patch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree {
            work: "/scratch/agent/work".into(),
            gitdir: "/scratch/agent/git".into(),
            home: "/scratch/agent/home".into(),
            tmp: "/scratch/agent/tmp".into(),
        }
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_cli_gets_the_guest_path_and_a_message_cannot_replace_it() {
        let proxy: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let requested = vec![
            ("PATH".to_string(), "/tmp/evil".to_string()),
            ("EXTRA".to_string(), "kept".to_string()),
        ];
        let env = env_for_cli(&tree(), proxy, &requested);
        assert_eq!(value(&env, "PATH"), Some("/usr/bin:/bin"));
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert_eq!(value(&env, "EXTRA"), Some("kept"));
    }
}
