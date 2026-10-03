//! The guest's request handlers. Each mirrors the 3a host worker (`FixtureExecutor`) step for
//! step, so results and `Refused.reason` strings are the same for the same tree. Sync and
//! tokio-free: the guest has no runtime.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agentos_core::contract::path_matches;
use agentos_core::guest::{
    b64, raw_frames_for, read_frame, Frame, Message, PatchStateKind, FILE_LIMIT, JSON_FRAME_LIMIT, OUTPUT_LIMIT, PROFILE_LIMIT,
    RAW_FRAME_LIMIT, SNAPSHOT_BYTES_LIMIT, SNAPSHOT_FILES_LIMIT,
};
use agentos_core::ids::Digest;
use agentos_core::patchrules::{check_summary, parse_numstat};
use agentos_core::workspace::{
    copy_tree, excluded_entries, has_excluded_component, list_files, purge_excluded, symlink_on_path, workspace_digest,
};
use rustix::process::{kill_process_group, waitpgid, Pid, Signal, WaitOptions};
use serde::Deserialize;

use crate::backend::Backend;

/// Why a streamed request failed: `Refused` is answered with that 3a reason; `Protocol`
/// means the peer broke the framing rules and the connection is closed without a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    Refused(String),
    Protocol(String),
}

/// The protected profile as received from the host, staged on disk only by
/// `run_verification`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedProfile {
    pub files: Vec<(String, Vec<u8>)>,
}

/// A finished check, with at most `OUTPUT_LIMIT` bytes of each stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub profile_id: String,
    pub command: Vec<String>,
    pub profile_digest: Digest,
    pub workspace_digest: Digest,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

impl Verified {
    pub fn into_message(self) -> Message {
        Message::Verified {
            profile_id: self.profile_id,
            command: self.command,
            profile_digest: self.profile_digest,
            workspace_digest: self.workspace_digest,
            exit_code: self.exit_code,
            stdout_b64: b64(&self.stdout),
            stdout_truncated: self.stdout_truncated,
            stderr_b64: b64(&self.stderr),
            stderr_truncated: self.stderr_truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchStateIs {
    pub state: PatchStateKind,
    pub paths: Vec<String>,
    pub workspace_digest: Option<Digest>,
    pub reason: Option<String>,
}

impl PatchStateIs {
    fn unknown(workspace_digest: Option<Digest>, reason: String) -> PatchStateIs {
        PatchStateIs { state: PatchStateKind::Unknown, paths: Vec::new(), workspace_digest, reason: Some(reason) }
    }

    pub fn into_message(self) -> Message {
        Message::PatchStateIs {
            state: self.state,
            paths: self.paths,
            workspace_digest: self.workspace_digest,
            reason: self.reason,
        }
    }
}

const WORKSPACE_MISSING: &str = "workspace missing: no snapshot was read";

/// Where streamed files go.
trait FileSink {
    fn begin(&mut self, rel: &str) -> io::Result<()>;
    fn chunk(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn end(&mut self) -> io::Result<()>;
}

struct DirSink {
    root: PathBuf,
    current: Option<File>,
}

impl FileSink for DirSink {
    fn begin(&mut self, rel: &str) -> io::Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.current = Some(File::create(path)?);
        Ok(())
    }

    fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.current.as_mut().ok_or_else(|| io::Error::other("no open file"))?.write_all(bytes)
    }

    fn end(&mut self) -> io::Result<()> {
        self.current.take();
        Ok(())
    }
}

#[derive(Default)]
struct MemSink {
    files: Vec<(String, Vec<u8>)>,
}

impl FileSink for MemSink {
    fn begin(&mut self, rel: &str) -> io::Result<()> {
        self.files.push((rel.to_string(), Vec::new()));
        Ok(())
    }

    fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.files.last_mut().ok_or_else(|| io::Error::other("no open file"))?.1.extend_from_slice(bytes);
        Ok(())
    }

    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A host-supplied path is only ever a plain relative path: no root, no `.`/`..`.
fn safe_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
        && !rel.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

/// Receives `File{path, len}` + `raw_frames_for(len)` raw frames per file, then `EndFiles`,
/// enforcing the announced count and bytes and the protocol limits. A sink error is
/// remembered and the rest of the stream is still drained, so the connection stays in step
/// and the request can be refused; the returned `Ok(Some(err))` carries it.
fn receive_files(
    link: &mut impl Read,
    file_count: u64,
    total_bytes: u64,
    bytes_limit: u64,
    sink: &mut dyn FileSink,
) -> Result<Option<io::Error>, String> {
    if file_count > SNAPSHOT_FILES_LIMIT {
        return Err(format!("{file_count} files announced, limit {SNAPSHOT_FILES_LIMIT}"));
    }
    if total_bytes > bytes_limit {
        return Err(format!("{total_bytes} bytes announced, limit {bytes_limit}"));
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut failed: Option<io::Error> = None;
    loop {
        let frame = read_frame(link, RAW_FRAME_LIMIT).map_err(|e| e.to_string())?;
        let (path, len) = match frame {
            Frame::Json(Message::EndFiles) => {
                if files != file_count || bytes != total_bytes {
                    return Err(format!(
                        "EndFiles after {files} files and {bytes} bytes, {file_count} and {total_bytes} announced"
                    ));
                }
                return Ok(failed);
            }
            Frame::Json(Message::File { path, len }) => (path, len),
            Frame::Json(m) => return Err(format!("unexpected message in a file stream: {m:?}")),
            Frame::Raw(_) => return Err("raw frame where a File was expected".into()),
        };
        files += 1;
        if files > file_count {
            return Err(format!("more than the {file_count} files announced"));
        }
        if len > FILE_LIMIT {
            return Err(format!("file of {len} bytes, limit {FILE_LIMIT}"));
        }
        bytes = bytes.checked_add(len).filter(|b| *b <= total_bytes).ok_or("more bytes than announced")?;
        if !safe_relative(&path) {
            return Err(format!("unsafe path {path:?}"));
        }
        if failed.is_none() {
            failed = sink.begin(&path).err();
        }
        let mut left = len;
        for _ in 0..raw_frames_for(len) {
            let want = left.min(RAW_FRAME_LIMIT as u64);
            let Frame::Raw(chunk) = read_frame(link, RAW_FRAME_LIMIT).map_err(|e| e.to_string())? else {
                return Err("JSON frame inside a file's bytes".into());
            };
            if chunk.len() as u64 != want {
                return Err(format!("raw frame of {} bytes, expected {want}", chunk.len()));
            }
            left -= want;
            if failed.is_none() {
                failed = sink.chunk(&chunk).err();
            }
        }
        if failed.is_none() {
            failed = sink.end().err();
        }
    }
}

/// `ReadSnapshot`: a clean workspace, the streamed files written into it, made durable, then
/// listed and digested (the host's `{"files","workspace_digest"}`).
pub fn read_snapshot(
    backend: &mut dyn Backend,
    link: &mut impl Read,
    file_count: u64,
    total_bytes: u64,
) -> Result<(Vec<String>, Digest), StreamError> {
    if file_count > SNAPSHOT_FILES_LIMIT || total_bytes > SNAPSHOT_BYTES_LIMIT {
        return Err(StreamError::Protocol(format!(
            "snapshot of {file_count} files and {total_bytes} bytes is over the limits"
        )));
    }
    let failed = |e: String| StreamError::Refused(format!("snapshot failed: {e}"));
    let prepared = backend.prepare_workspace();
    let ws = backend.workspace_dir().to_path_buf();
    let mut dir_sink = DirSink { root: ws.clone(), current: None };
    // Even when the workspace could not be prepared, the stream is drained, so the reply
    // stays in step with the request.
    let mut discard = DiscardSink;
    let sink: &mut dyn FileSink = if prepared.is_ok() { &mut dir_sink } else { &mut discard };
    let written = receive_files(link, file_count, total_bytes, SNAPSHOT_BYTES_LIMIT, sink).map_err(StreamError::Protocol)?;
    prepared.map_err(failed)?;
    if let Some(e) = written {
        return Err(failed(e.to_string()));
    }
    // Written as root; `git` runs as `builder` from now on.
    backend.own_tree(&ws).map_err(failed)?;
    backend.sync_workspace().map_err(failed)?;
    let files = list_files(&ws).map_err(|e| failed(e.to_string()))?.into_iter().map(|(rel, _)| rel).collect();
    let digest = workspace_digest(&ws).map_err(|e| failed(e.to_string()))?;
    Ok((files, digest))
}

/// Drops everything (used when the destination is unusable but the stream must be drained).
struct DiscardSink;

impl FileSink for DiscardSink {
    fn begin(&mut self, _: &str) -> io::Result<()> {
        Ok(())
    }
    fn chunk(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The profile files of a `RunVerification`, held in memory (≤ `PROFILE_LIMIT`).
pub fn receive_profile(link: &mut impl Read, file_count: u64, total_bytes: u64) -> Result<StagedProfile, StreamError> {
    let mut sink = MemSink::default();
    receive_files(link, file_count, total_bytes, PROFILE_LIMIT, &mut sink).map_err(StreamError::Protocol)?;
    Ok(StagedProfile { files: sink.files })
}

/// Empties `dir` but keeps the directory itself (its owner and mode: in a VM
/// `/scratch/check` is `check`'s, 0700); creates it if missing. `remove_dir_all` never
/// follows a symlink the previous check may have planted.
fn empty_dir(dir: &Path) -> io::Result<()> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if fs::symlink_metadata(&path)?.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir_all(dir),
        Err(e) => Err(e),
    }
}

/// A fresh, empty `<scratch>/<name>`.
fn fresh_scratch(backend: &dyn Backend, name: &str) -> Result<PathBuf, String> {
    let dir = backend.scratch_dir().join(name);
    let made = || -> io::Result<()> {
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::create_dir_all(&dir)
    };
    made().map_err(|e| format!("scratch dir: {e}"))?;
    Ok(dir)
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

/// Repo-relative paths `patch_file` touches, in patch order, or why it is unacceptable
/// (3a `paths_of_file`). `cwd` lies outside any repository.
fn paths_of_file(backend: &dyn Backend, patch_file: &Path, cwd: &Path) -> Result<Vec<String>, String> {
    let numstat = backend
        .git(cwd)
        .args(["apply", "--numstat", "-z"])
        .arg(patch_file)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !numstat.status.success() {
        return Err(format!("invalid patch: {}", stderr_of(&numstat)));
    }
    let summary =
        backend.git(cwd).args(["apply", "--summary"]).arg(patch_file).output().map_err(|e| format!("cannot run git: {e}"))?;
    if !summary.status.success() {
        return Err(format!("invalid patch: {}", stderr_of(&summary)));
    }
    check_summary(&summary.stdout)?;
    parse_numstat(&numstat.stdout)
}

/// Writes the patch to a fresh `<scratch>/<name>/change.patch` and parses it there.
fn stage_patch(backend: &dyn Backend, name: &str, patch: &[u8]) -> Result<(PathBuf, PathBuf, Vec<String>), String> {
    let dir = fresh_scratch(backend, name)?;
    let file = dir.join("change.patch");
    fs::write(&file, patch).map_err(|e| format!("cannot write patch: {e}"))?;
    let paths = paths_of_file(backend, &file, &dir)?;
    Ok((dir, file, paths))
}

/// `ApplyPatch`, in 3a's validation order: parse, editable paths, excluded components,
/// symlinks, expected base; only then git.
pub fn apply_patch(
    backend: &mut dyn Backend,
    expected_base: Digest,
    editable_paths: &[String],
    patch: &[u8],
) -> Result<(Vec<String>, Digest), String> {
    if !backend.workspace_present() {
        return Err(WORKSPACE_MISSING.into());
    }
    let ws = backend.workspace_dir().to_path_buf();
    let (_dir, patch_file, paths) = stage_patch(backend, "patch", patch)?;
    if let Some(p) = paths.iter().find(|p| !path_matches(editable_paths, p)) {
        return Err(format!("path not editable: {p}"));
    }
    if let Some(p) = paths.iter().find(|p| has_excluded_component(p)) {
        return Err(format!("path excluded from the workspace digest: {p}"));
    }
    for p in &paths {
        match symlink_on_path(&ws, p) {
            Ok(None) => {}
            Ok(Some(link)) => return Err(format!("path {p} crosses symlink {link}")),
            Err(e) => return Err(format!("cannot inspect {p}: {e}")),
        }
    }
    let actual = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;
    if actual != expected_base {
        return Err(format!("version conflict: expected {expected_base}, actual {actual}"));
    }
    // A planted `.git` would make `git apply` honour its repo-local configuration.
    purge_excluded(&ws).map_err(|e| format!("cannot clean workspace: {e}"))?;
    for args in [&["apply", "--check"][..], &["apply"][..]] {
        let out = backend.git(&ws).args(args).arg(&patch_file).output().map_err(|e| format!("cannot run git: {e}"))?;
        if !out.status.success() {
            return Err(format!("patch does not apply: {}", stderr_of(&out)));
        }
    }
    backend.sync_workspace().map_err(|e| format!("cannot sync workspace: {e}"))?;
    let digest = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;
    Ok((paths, digest))
}

#[derive(Deserialize)]
struct Profile {
    id: String,
    command: Vec<String>,
}

/// The serialized `command` and `profile_id` must leave room in the `Verified` frame for
/// both output streams at their limit (base64) and the fixed fields.
pub const VERIFIED_TEXT_BUDGET: usize = JSON_FRAME_LIMIT - 2 * (4 * (OUTPUT_LIMIT + 1).div_ceil(3)) - 4096;

/// Refuses a profile whose `id` and `command`, as JSON, would not fit `Verified`.
fn check_reply_size(profile: &Profile) -> Result<(), String> {
    let size = serde_json::to_vec(&profile.command).map_or(usize::MAX, |v| v.len())
        + serde_json::to_vec(&profile.id).map_or(usize::MAX, |v| v.len());
    if size > VERIFIED_TEXT_BUDGET {
        return Err(format!("profile command too large: {size} bytes of JSON, limit {VERIFIED_TEXT_BUDGET}"));
    }
    Ok(())
}

fn cut(mut bytes: Vec<u8>) -> (Vec<u8>, bool) {
    let truncated = bytes.len() > OUTPUT_LIMIT;
    bytes.truncate(OUTPUT_LIMIT);
    (bytes, truncated)
}

/// `RunVerification`: stage the profile fresh under `<scratch>/profile`, check its digest
/// against the pin, parse `profile.json` from the staged bytes, purge and digest the
/// workspace, run the check in its own process group, then void the evidence if the staged
/// profile or the workspace changed or the check left excluded entries behind.
pub fn run_verification(
    backend: &mut dyn Backend,
    pinned: Option<Digest>,
    timeout_secs: u64,
    profile_files: StagedProfile,
) -> Result<Verified, String> {
    if !backend.workspace_present() {
        return Err(WORKSPACE_MISSING.into());
    }
    let ws = backend.workspace_dir().to_path_buf();
    let staged = fresh_scratch(backend, "profile")?;
    let mut sink = DirSink { root: staged.clone(), current: None };
    for (rel, bytes) in &profile_files.files {
        if !safe_relative(rel) {
            return Err(format!("cannot stage profile: unsafe path {rel:?}"));
        }
        sink.begin(rel)
            .and_then(|()| sink.chunk(bytes))
            .and_then(|()| sink.end())
            .map_err(|e| format!("cannot stage profile: {e}"))?;
    }
    drop(profile_files);
    let profile_digest = workspace_digest(&staged).map_err(|e| format!("cannot digest profile: {e}"))?;
    if let Some(pinned) = pinned
        && pinned != profile_digest
    {
        return Err(format!("profile digest mismatch: pinned {pinned}, found {profile_digest}"));
    }
    // Parsed from the bytes just digested.
    let raw = fs::read(staged.join("profile.json")).map_err(|e| format!("cannot read profile: {e}"))?;
    let profile: Profile = serde_json::from_slice(&raw).map_err(|e| format!("invalid profile.json: {e}"))?;
    let Some((program, args)) = profile.command.split_first() else {
        return Err("profile command is empty".into());
    };
    check_reply_size(&profile)?;
    // Entries the digest ignores must not decide the check.
    purge_excluded(&ws).map_err(|e| format!("cannot clean workspace: {e}"))?;
    let workspace = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;

    // The check's own scratch starts empty on every run, also within one boot: no bytecode
    // or file a previous check left behind may influence this one.
    let check_dir = backend.scratch_dir().join("check");
    empty_dir(&check_dir).map_err(|e| format!("scratch dir: {e}"))?;
    let pycache = check_dir.join("pycache");
    backend.check_program(program, &staged).map_err(|e| format!("cannot run profile command: {e}"))?;
    let cmd = backend.check_command(program, args, &ws, &staged, &pycache);
    let output = match run_in_group(cmd, Duration::from_secs(timeout_secs), OUTPUT_LIMIT) {
        Err(GroupError::Timeout) => return Err("timeout".into()),
        Err(GroupError::Io(e)) => return Err(format!("cannot run profile command: {e}")),
        Ok(o) => o,
    };

    let unchanged = |dir: &Path, want: Digest| workspace_digest(dir).is_ok_and(|d| d == want);
    if !unchanged(&staged, profile_digest) {
        return Err("protected profile changed during verification".into());
    }
    if !unchanged(&ws, workspace) {
        return Err("workspace changed during verification".into());
    }
    let polluted = excluded_entries(&ws).map_err(|e| format!("cannot inspect workspace: {e}"))?;
    if !polluted.is_empty() {
        return Err(format!("workspace polluted by excluded entries: {}", polluted.join(", ")));
    }

    let (stdout, stdout_truncated) = cut(output.stdout);
    let (stderr, stderr_truncated) = cut(output.stderr);
    Ok(Verified {
        profile_id: profile.id,
        command: profile.command,
        profile_digest,
        workspace_digest: workspace,
        exit_code: output.status.code(),
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
    })
}

/// `Digest` (inspect).
pub fn digest(backend: &dyn Backend) -> Result<Digest, String> {
    if !backend.workspace_present() {
        return Err("workspace missing".into());
    }
    workspace_digest(backend.workspace_dir()).map_err(|e| e.to_string())
}

/// `PatchState` (inspect), 3a `patch_state`: the base ⇒ `not_applied`; the base once this
/// patch is reverted on a scratch copy ⇒ `applied`; anything else ⇒ `unknown`.
pub fn patch_state(backend: &dyn Backend, expected_base: Digest, patch: &[u8]) -> PatchStateIs {
    if !backend.workspace_present() {
        return PatchStateIs::unknown(None, "workspace missing".into());
    }
    let ws = backend.workspace_dir();
    let actual = match workspace_digest(ws) {
        Ok(d) => d,
        Err(e) => return PatchStateIs::unknown(None, format!("cannot digest workspace: {e}")),
    };
    if actual == expected_base {
        return PatchStateIs { state: PatchStateKind::NotApplied, paths: Vec::new(), workspace_digest: Some(actual), reason: None };
    }
    let (dir, patch_file, paths) = match stage_patch(backend, "reverse", patch) {
        Ok(staged) => staged,
        Err(reason) => return PatchStateIs::unknown(Some(actual), reason),
    };
    let copy = dir.join("ws");
    if let Err(e) = copy_tree(ws, &copy) {
        return PatchStateIs::unknown(Some(actual), format!("cannot copy workspace: {e}"));
    }
    if let Err(e) = backend.own_tree(&copy) {
        return PatchStateIs::unknown(Some(actual), e);
    }
    let reverted = match backend.git(&copy).args(["apply", "--reverse"]).arg(&patch_file).output() {
        Ok(out) => out.status.success() && workspace_digest(&copy).is_ok_and(|d| d == expected_base),
        Err(e) => return PatchStateIs::unknown(Some(actual), format!("cannot run git: {e}")),
    };
    if !reverted {
        return PatchStateIs::unknown(
            Some(actual),
            format!("workspace {actual} is neither the base {expected_base} nor the base with this patch"),
        );
    }
    PatchStateIs { state: PatchStateKind::Applied, paths, workspace_digest: Some(actual), reason: None }
}

struct GroupOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum GroupError {
    Timeout,
    Io(io::Error),
}

/// Keeps the first `limit + 1` bytes and drains the rest, so a chatty child never blocks.
fn capture(mut pipe: impl Read + Send + 'static, limit: usize) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut pipe).take(limit as u64 + 1).read_to_end(&mut kept);
        let _ = io::copy(&mut pipe, &mut io::sink());
        kept
    })
}

fn kill_group(pgid: Option<Pid>) {
    if let Some(pgid) = pgid {
        // ESRCH (the group is already gone) is fine.
        let _ = kill_process_group(pgid, Signal::KILL);
    }
}

/// How long `reap_group` waits for killed members to become reapable.
const REAP_WINDOW: Duration = Duration::from_secs(1);

/// Reaps the killed group's members that were re-parented to this process. In the VM the
/// agent is PID 1, so every orphan of the check lands here, and an unreaped zombie of `check`
/// would keep counting against its `RLIMIT_NPROC` for the next run. Only this group is
/// waited for (never "any child", which would steal the statuses of `git` and other children
/// std is waiting on). Elsewhere (the fake) orphans go to the real init: `ECHILD` at once.
fn reap_group(pgid: Option<Pid>) {
    let Some(pgid) = pgid else { return };
    let deadline = std::time::Instant::now() + REAP_WINDOW;
    loop {
        match waitpgid(pgid, WaitOptions::NOHANG) {
            Ok(Some(_)) => {}
            // Members of ours still dying.
            Ok(None) if std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            // ECHILD: none left; or the window is over.
            Ok(None) | Err(_) => return,
        }
    }
}

/// Runs `cmd` as the leader of a new process group (3a `run_in_group`, without tokio): a
/// waiter thread reaps the leader; when it exits or the timeout fires, the whole group is
/// killed, so no grandchild outlives the run or holds the pipes open.
fn run_in_group(mut cmd: std::process::Command, timeout: Duration, limit: usize) -> Result<GroupOutput, GroupError> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child: Child = cmd.spawn().map_err(GroupError::Io)?;
    let pgid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let stdout = capture(child.stdout.take().expect("stdout is piped"), limit);
    let stderr = capture(child.stderr.take().expect("stderr is piped"), limit);

    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(());
        status
    });
    let timed_out = matches!(rx.recv_timeout(timeout), Err(mpsc::RecvTimeoutError::Timeout));
    kill_group(pgid);
    let status = waiter.join().map_err(|_| GroupError::Io(io::Error::other("waiter thread panicked")))?;
    // Only now that std has reaped the leader (waiting on the group earlier could steal its
    // status).
    reap_group(pgid);
    if timed_out {
        return Err(GroupError::Timeout);
    }
    let status = status.map_err(GroupError::Io)?;
    let join = |h: thread::JoinHandle<Vec<u8>>| h.join().map_err(|_| GroupError::Io(io::Error::other("capture thread panicked")));
    Ok(GroupOutput { status, stdout: join(stdout)?, stderr: join(stderr)? })
}
