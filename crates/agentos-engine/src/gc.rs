//! Conservative collection of reconstructible transient data; see
//! `docs/superpowers/specs/2026-10-04-conservative-gc-design.md`.
//!
//! A pass runs under the home driver lock, proved by [`HeldDriverLock`]. It first scans
//! and classifies every owned entry, task by task, deleting nothing; an integrity problem
//! (a symlinked or unknown path, a mount root, a bad deletion ticket, a corrupt blob) stops
//! the pass there. Each task is then collected all-or-nothing: anything that may still be
//! live (an unfinished task, a held job or workspace lock, inspection or jail leftovers,
//! a tree the collector will not walk) retains that task only. Tasks are revalidated and
//! deleted in batches of whole tasks; descriptors are held only for the batch in flight.
//!
//! No journal, input, registry or blob is removed. In a job directory only `output.bin`
//! (a copy of the published result blob), `scratch.img` and a Firecracker job's `v.sock`
//! are removed; logs, status, requests, receipts and outcomes stay.
mod confined;

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_store::{blob::BlobStore, db::Db};
use rustix::fs::{FileType, Mode, OFlags, openat};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::executor::ExecOutcome;
use crate::job::{JobRequest, WorkerConfig};

/// Default number of entries revalidated and deleted together (whole tasks per batch).
pub const DEFAULT_BATCH_SIZE: usize = 64;
/// Most tasks (each holding its `ws.lock`) revalidated and deleted together, whatever the
/// batch size; also capped by the descriptor limit (see `tasks_per_batch`).
pub const MAX_TASKS_PER_BATCH: usize = 32;
/// Descriptors kept free for everything but held workspace locks and the remover's levels:
/// the home, database, staging, ticket, parent and job-lock descriptors and their listings.
const FD_RESERVE: usize = 32;
/// Most entries listed in a report; the summary always counts every entry.
pub const REPORT_LIMIT: usize = 1000;
const TREE_LIMIT: usize = 10000;
const JSON_LIMIT: u64 = 32 * 1024 * 1024;
const REASON_LIMIT: usize = 1024;
const TICKET_VERSION: u64 = 2;
const TICKET_TMP_PREFIX: &str = ".tmp-ticket-";
/// The only files removed from a job directory, once its receipt is the published result.
const JOB_FILES: [&str; 3] = ["output.bin", "scratch.img", "v.sock"];
const WORKSPACES: [&str; 3] = ["ws", "workspace", "ws.img"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Dry run: would be deleted.
    Candidate,
    Deleted,
    /// A job directory whose redundant copies are already gone; its evidence stays.
    Collected,
    /// Kept on purpose; not an error.
    Retained,
    /// Kept because of an integrity problem or a failure; the pass exits non-zero.
    Refused,
    /// Not processed because the pass stopped.
    Skipped,
}
impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Candidate => "candidate",
            Status::Deleted => "deleted",
            Status::Collected => "collected",
            Status::Retained => "retained",
            Status::Refused => "refused",
            Status::Skipped => "skipped",
        }
    }
}
impl PartialEq<&str> for Status {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub path: String,
    pub status: Status,
    pub reason: String,
}
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub candidate: usize,
    pub deleted: usize,
    pub collected: usize,
    pub retained: usize,
    pub refused: usize,
    pub skipped: usize,
}
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub batch_size: usize,
    /// The tasks-per-batch cap in force (fixed maximum, lowered by the descriptor limit).
    pub tasks_per_batch: usize,
    pub batches: usize,
    pub summary: Summary,
    /// Entries beyond [`REPORT_LIMIT`] were omitted (refused and skipped ones come first).
    pub truncated: bool,
    pub entries: Vec<Entry>,
}
impl Report {
    /// Whether anything was refused or skipped: the command then exits non-zero.
    pub fn failed(&self) -> bool {
        self.summary.refused > 0 || self.summary.skipped > 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dry_run: bool,
    pub batch_size: usize,
}
impl Options {
    pub fn new(dry_run: bool) -> Options {
        Options {
            dry_run,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// Proof that the caller holds `<home>/driver.lock`, so no controller, supervisor launch
/// or other collector runs in the home. The home root is canonicalized once here; every
/// later operation inside it is descriptor-relative and never follows a symlink.
pub struct HeldDriverLock<'a> {
    root: PathBuf,
    /// The home directory, opened once; the lock was checked through it and every
    /// collection operation starts from it, so proof and anchor are the same directory.
    anchor: File,
    _lock: &'a File,
}
impl<'a> HeldDriverLock<'a> {
    /// `lock` must be the open `driver.lock` of `root` on which the caller holds (or can
    /// take now) the exclusive lock.
    pub fn verify(root: &Path, lock: &'a File) -> io::Result<HeldDriverLock<'a>> {
        let root = fs::canonicalize(root)?;
        // The root was canonicalized once: from here on nothing follows a symlink.
        let anchor = fs::OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32)
            .open(&root)?;
        let on_disk = confined::stat(&anchor, OsStr::new("driver.lock"))?
            .ok_or_else(|| io::Error::other("the home has no driver.lock"))?;
        let held = lock.metadata()?;
        if confined::file_type(&on_disk) != FileType::RegularFile
            || confined::identity_of(&on_disk) != (held.dev(), held.ino())
        {
            return Err(io::Error::other(
                "the given file is not this home's driver.lock",
            ));
        }
        lock.try_lock()
            .map_err(|e| io::Error::other(format!("the driver lock is not held: {e}")))?;
        Ok(HeldDriverLock {
            root,
            anchor,
            _lock: lock,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Collection boundaries for interruption and substitution probes (tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStage {
    /// A batch was revalidated; nothing of it is deleted yet.
    Validated,
    /// A deletion ticket is durable; the entry has not moved.
    TicketPublished,
    /// The entry was moved under `gc-trash`; nothing of it is removed yet.
    Staged,
    /// The staged data and its directory are gone; the ticket remains.
    Removed,
    /// Staged data is about to be moved back to its original place.
    Restoring,
}

/// Why an entry or task is not collected.
#[derive(Debug)]
enum Fail {
    /// Retain the task; not an error.
    Retain(String),
    /// Retain the task and report a failure.
    Refuse(String),
    /// Like `Refuse`, for a resource shortage (descriptors, memory): staged data and its
    /// ticket stay as they are and the next pass finishes them.
    Transient(String),
    /// Stop the whole pass.
    Integrity(String),
}
impl Fail {
    fn text(&self) -> &str {
        match self {
            Fail::Retain(r) | Fail::Refuse(r) | Fail::Transient(r) | Fail::Integrity(r) => r,
        }
    }
}
impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Fail {
        if e.get_ref()
            .is_some_and(|inner| inner.is::<confined::MountBoundary>())
        {
            Fail::Integrity(reason(&e))
        } else if transient(&e) {
            Fail::Transient(format!(
                "{}: a resource shortage, not a problem with the data; retry with a higher \
                 descriptor limit (ulimit -n) or more free memory",
                reason(&e)
            ))
        } else {
            Fail::Refuse(reason(&e))
        }
    }
}
impl From<rustix::io::Errno> for Fail {
    fn from(e: rustix::io::Errno) -> Fail {
        io::Error::from(e).into()
    }
}

/// Descriptor or memory exhaustion: says nothing about the data, so never an integrity stop.
fn transient(e: &io::Error) -> bool {
    [
        rustix::io::Errno::MFILE,
        rustix::io::Errno::NFILE,
        rustix::io::Errno::NOMEM,
        rustix::io::Errno::NOBUFS,
    ]
    .iter()
    .any(|errno| e.raw_os_error() == Some(errno.raw_os_error()))
}

/// Tasks per batch: `MAX_TASKS_PER_BATCH`, lowered so that one held `ws.lock` per task,
/// one descriptor per removed tree level and `FD_RESERVE` fit the soft descriptor limit.
fn tasks_per_batch() -> usize {
    let soft = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(u64::MAX);
    let spare = soft.saturating_sub((confined::DEPTH_LIMIT + FD_RESERVE) as u64);
    usize::try_from(spare)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_TASKS_PER_BATCH)
}

/// A human-readable, bounded reason: descriptor paths are dropped and long text is cut.
fn reason(e: impl std::fmt::Display) -> String {
    const FD: &str = "/proc/self/fd/";
    let mut text = e.to_string();
    while let Some(at) = text.find(FD) {
        let digits = text[at + FD.len()..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let mut end = at + FD.len() + digits;
        if text[end..].starts_with('/') {
            end += 1;
        }
        text.replace_range(at..end, "");
    }
    if text.len() > REASON_LIMIT {
        let mut end = REASON_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

fn hex_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn uuid_name(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
/// `<effect>-<attempt>` as written by job and model retention.
fn attempt_name(name: &str) -> Option<EffectId> {
    if name.len() != 101 || !hex_name(&name[..64]) || &name[64..65] != "-" {
        return None;
    }
    if !uuid_name(&name[65..]) {
        return None;
    }
    serde_json::from_value(serde_json::Value::String(name[..64].into())).ok()
}
fn task_id(name: &str) -> Option<TaskId> {
    uuid_name(name)
        .then(|| serde_json::from_value(serde_json::Value::String(name.into())).ok())
        .flatten()
}
fn key_of(relative: &Path) -> String {
    Digest::of(relative.as_os_str().as_encoded_bytes()).to_string()
}
fn display(relative: &Path) -> String {
    relative.to_string_lossy().into_owned()
}

/// Opens `name` in `dir` for reading: never a symlink, a FIFO wait or a hard-linked file.
fn open_plain(dir: &File, name: &str) -> Result<File, String> {
    let file: File = openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("cannot open {name}: {e}"))?
    .into();
    let meta = file.metadata().map_err(|e| reason(&e))?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(format!("{name} is not a single-link regular file"));
    }
    Ok(file)
}
fn read_json_at<T: DeserializeOwned>(dir: &File, name: &str) -> Result<T, String> {
    let file = open_plain(dir, name)?;
    let mut bytes = Vec::new();
    file.take(JSON_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| reason(&e))?;
    if bytes.len() as u64 > JSON_LIMIT {
        return Err(format!("{name} is larger than 32 MiB"));
    }
    // Never quote the content: a malformed file may hold anything.
    serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "malformed {name}: {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        )
    })
}
fn read_file_at(dir: &File, name: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    open_plain(dir, name)?
        .read_to_end(&mut bytes)
        .map_err(|e| reason(&e))?;
    Ok(bytes)
}

/// A failure to open an owned path: a symlink or non-directory on the way is an
/// integrity problem, anything else a refusal.
fn path_error(e: io::Error) -> Fail {
    let redirected = [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR]
        .iter()
        .any(|errno| e.raw_os_error() == Some(errno.raw_os_error()));
    if redirected {
        Fail::Integrity(
            "not a directory where an owned one is expected (symlinks are refused)".into(),
        )
    } else {
        e.into()
    }
}

/// `name` in `parent` as an owned directory: a symlink or anything else there is an
/// integrity problem.
fn owned_dir(parent: &File, name: &str) -> Result<File, Fail> {
    confined::open_dir(parent, OsStr::new(name)).map_err(|e| match path_error(e) {
        Fail::Integrity(_) => {
            Fail::Integrity(format!("{name} is not a directory (symlinks are refused)"))
        }
        other => other,
    })
}
fn owned_dir_opt(parent: &File, name: &str) -> Result<Option<File>, Fail> {
    match confined::stat(parent, OsStr::new(name))? {
        None => Ok(None),
        Some(st) if confined::file_type(&st) == FileType::Directory => {
            owned_dir(parent, name).map(Some)
        }
        Some(_) => Err(Fail::Integrity(format!(
            "{name} is not a directory (symlinks are refused)"
        ))),
    }
}

/// A descriptor path naming the directory `dir` itself (the bare `/proc/self/fd/N` is a
/// symlink to it).
fn pinned(dir: &File) -> PathBuf {
    confined::path(dir).join(".")
}

#[derive(Clone, Copy, Default)]
struct Rules {
    /// The tree root itself may be a socket (a staged `v.sock`).
    socket_root: bool,
    /// A top-level socket of this name is allowed (a Firecracker job's `v.sock`).
    socket_child: Option<&'static str>,
}

/// Walks the tree at `root` (a descriptor path) before anything of it is removed. Data
/// the collector will not remove blindly (symlinks, special files, hard links, too many
/// entries or levels) retains the task; a mount root is an integrity problem.
fn check_tree(root: &Path, rules: Rules) -> Result<(), Fail> {
    let named = |path: &Path| match path.strip_prefix(root) {
        Ok(p) if p.as_os_str().is_empty() => ".".to_string(),
        Ok(p) => reason(p.display()),
        Err(_) => "?".to_string(),
    };
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut count = 0usize;
    while let Some((path, depth)) = stack.pop() {
        count += 1;
        if count > TREE_LIMIT {
            return Err(Fail::Retain(format!(
                "tree has more than {TREE_LIMIT} entries"
            )));
        }
        if depth > confined::DEPTH_LIMIT {
            return Err(Fail::Retain(format!(
                "tree is deeper than {} levels",
                confined::DEPTH_LIMIT
            )));
        }
        confined::refuse_mount(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        if ft.is_dir() {
            for entry in fs::read_dir(&path)? {
                if count + stack.len() >= TREE_LIMIT {
                    return Err(Fail::Retain(format!(
                        "tree has more than {TREE_LIMIT} entries"
                    )));
                }
                stack.push((entry?.path(), depth + 1));
            }
        } else if ft.is_socket()
            && meta.nlink() == 1
            && ((depth == 0 && rules.socket_root)
                || (depth == 1
                    && rules
                        .socket_child
                        .is_some_and(|n| path.file_name() == Some(OsStr::new(n)))))
        {
            // A Firecracker worker leaves its owned socket after shutting the VM down.
        } else if ft.is_symlink() {
            return Err(Fail::Retain(format!(
                "symlink in the tree: {}",
                named(&path)
            )));
        } else if !ft.is_file() {
            return Err(Fail::Retain(format!(
                "special file in the tree: {}",
                named(&path)
            )));
        } else if meta.nlink() != 1 {
            return Err(Fail::Retain(format!(
                "hard-linked file in the tree: {}",
                named(&path)
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    /// One redundant file of a job directory (`JOB_FILES`).
    JobFile,
    /// A whole `model/<effect>-<attempt>` response retention.
    Model,
    /// `work/<task>/{ws,workspace,ws.img}`.
    Workspace,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Proof {
    task: TaskId,
    effect: Option<EffectId>,
    attempt: Option<AttemptId>,
    result: Option<Digest>,
    lease: u64,
    firecracker_socket: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ticket {
    version: u64,
    relative: PathBuf,
    kind: Kind,
    proof: Proof,
    device: u64,
    inode: u64,
    /// The staged data was not validated (it changed between validation and the move) and
    /// moving it back failed: a later pass moves it back and never removes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    restore: bool,
    /// The kind of file that was staged, from the same stat that captured `inode`.
    /// Absent in tickets of earlier builds (see `ticket_accepts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_type: Option<StagedType>,
}

/// A kind of file as recorded in a ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StagedType {
    Directory,
    RegularFile,
    Socket,
    Symlink,
    Fifo,
    CharDevice,
    BlockDevice,
    Unknown,
}
impl StagedType {
    fn of(ft: FileType) -> StagedType {
        match ft {
            FileType::Directory => StagedType::Directory,
            FileType::RegularFile => StagedType::RegularFile,
            FileType::Socket => StagedType::Socket,
            FileType::Symlink => StagedType::Symlink,
            FileType::Fifo => StagedType::Fifo,
            FileType::CharacterDevice => StagedType::CharDevice,
            FileType::BlockDevice => StagedType::BlockDevice,
            FileType::Unknown => StagedType::Unknown,
        }
    }
}

/// THE rule for which staged data a ticket describes (by kind of file; identity is
/// compared separately). Every ticket writer goes through `removal_ticket` or
/// `restore_ticket` and every reader through this function:
/// - a removal ticket only ever describes the expected kind for its entry
///   (`expected_type`), and a recorded type must be that kind too;
/// - a restore ticket describes whatever was staged, as recorded: it only ever moves data
///   back, so any recorded kind is safe; one without a record (earlier builds) keeps the
///   expected-kind rule.
fn ticket_accepts(ticket: &Ticket, staged: FileType) -> bool {
    let name = ticket
        .relative
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    let expected = StagedType::of(expected_type(ticket.kind, name));
    let actual = StagedType::of(staged);
    match (ticket.restore, ticket.file_type) {
        (true, Some(recorded)) => actual == recorded,
        (_, recorded) => actual == expected && recorded.is_none_or(|r| r == expected),
    }
}

/// The ticket that stages `d` for removal, recording the kind of file validated.
fn removal_ticket(d: &Delete) -> Ticket {
    Ticket {
        version: TICKET_VERSION,
        relative: d.relative.clone(),
        kind: d.kind,
        proof: d.proof.clone(),
        device: d.identity.0,
        inode: d.identity.1,
        restore: false,
        file_type: Some(StagedType::of(d.file_type)),
    }
}

/// `ticket` rewritten to name staged data (moved by this pass) for moving back only.
fn restore_ticket(ticket: &Ticket, identity: (u64, u64), staged: FileType) -> Ticket {
    Ticket {
        device: identity.0,
        inode: identity.1,
        restore: true,
        file_type: Some(StagedType::of(staged)),
        ..ticket.clone()
    }
}

impl Ticket {
    fn rules(&self) -> Rules {
        Rules {
            socket_root: self.kind == Kind::JobFile
                && self.proof.firecracker_socket
                && self.relative.file_name() == Some(OsStr::new("v.sock")),
            socket_child: None,
        }
    }
}

/// The only kind of file an entry of `kind` named `name` may be: classification stages
/// nothing else, and a later pass recognises staged data by it.
fn expected_type(kind: Kind, name: &str) -> FileType {
    match (kind, name) {
        (Kind::JobFile, "v.sock") => FileType::Socket,
        (Kind::JobFile, _) | (Kind::Workspace, "ws.img") => FileType::RegularFile,
        (Kind::Workspace, _) | (Kind::Model, _) => FileType::Directory,
    }
}

/// Only the fixed shapes this build writes; anything else is never executed.
fn ticket_shape(ticket: &Ticket) -> Result<(), String> {
    let parts: Vec<&OsStr> = ticket
        .relative
        .components()
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            _ => Err("deletion ticket path is not a plain relative path".to_string()),
        })
        .collect::<Result<_, _>>()?;
    let proof = &ticket.proof;
    let owned = |effect: &Option<EffectId>, attempt: &Option<AttemptId>| match (effect, attempt) {
        (Some(e), Some(a)) => Some(format!("{e}-{a}")),
        _ => None,
    };
    let ok = match ticket.kind {
        Kind::Workspace => {
            parts.len() == 3
                && parts[0] == "work"
                && parts[1] == proof.task.as_str()
                && WORKSPACES.iter().any(|n| parts[2] == *n)
                && proof.effect.is_none()
                && proof.result.is_none()
        }
        Kind::JobFile => {
            parts.len() == 3
                && parts[0] == "jobs"
                && owned(&proof.effect, &proof.attempt).is_some_and(|o| parts[1] == o.as_str())
                && JOB_FILES.iter().any(|n| parts[2] == *n)
                && (parts[2] != "v.sock" || proof.firecracker_socket)
                && proof.result.is_some()
        }
        Kind::Model => {
            parts.len() == 2
                && parts[0] == "model"
                && owned(&proof.effect, &proof.attempt).is_some_and(|o| parts[1] == o.as_str())
                && proof.result.is_some()
        }
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "deletion ticket for {} does not match its kind and owner",
            display(&ticket.relative)
        ))
    }
}

struct Delete {
    relative: PathBuf,
    kind: Kind,
    proof: Proof,
    /// Device/inode of the entry, captured while it was validated.
    identity: (u64, u64),
    /// Its kind of file, from the same stat.
    file_type: FileType,
    /// Device/inode of its parent directory, captured while it was validated.
    parent: (u64, u64),
}
enum Action {
    Delete(Delete),
    /// Staged data (or none left) whose ticket is valid: finish removing it.
    Finish {
        key: String,
        ticket: Ticket,
    },
    /// A ticket whose entry never moved: drop it; the entry is classified afresh.
    Retire {
        key: String,
    },
    /// Staged data that is not what its ticket proved: move it back.
    Restore {
        key: String,
        ticket: Ticket,
    },
}
impl Action {
    fn path(&self) -> String {
        match self {
            Action::Delete(d) => display(&d.relative),
            Action::Finish { ticket, .. } | Action::Restore { ticket, .. } => {
                display(&ticket.relative)
            }
            Action::Retire { key } => format!("gc-trash/{key}.json"),
        }
    }
    fn planned(&self) -> &'static str {
        match self {
            Action::Delete(_) => "settled redundant copy; evidence and durable artifacts kept",
            Action::Finish { .. } => "interrupted deletion of a proven copy will be finished",
            Action::Retire { .. } => "unexecuted deletion ticket will be dropped; data kept",
            Action::Restore { .. } => {
                "staged data does not match its deletion ticket; it will be moved back"
            }
        }
    }
}

#[derive(Default)]
struct Items {
    pending: Vec<(String, Ticket)>,
    jobs: Vec<String>,
    models: Vec<String>,
    workspaces: Vec<&'static str>,
}
impl Items {
    fn paths(&self, task: &TaskId) -> Vec<String> {
        let mut paths: Vec<String> = self
            .pending
            .iter()
            .map(|(_, t)| display(&t.relative))
            .collect();
        paths.extend(self.jobs.iter().map(|j| format!("jobs/{j}")));
        paths.extend(self.models.iter().map(|m| format!("model/{m}")));
        paths.extend(self.workspaces.iter().map(|w| format!("work/{task}/{w}")));
        paths
    }
}
struct Scan {
    tasks: BTreeMap<String, (TaskId, Items)>,
    tmp_tickets: Vec<String>,
}

/// A held `ws.lock` and the device/inode of the `work/<task>` directory holding it.
type WsLock = (File, (u64, u64));

struct TaskPlan {
    /// The held workspace lock and the identity of `work/<task>` it lives in.
    lock: Option<WsLock>,
    /// (path, failure) that retains the whole task.
    blocked: Option<(String, Fail)>,
    actions: Vec<Action>,
    notes: Vec<Entry>,
}

/// An integrity problem: the whole pass stops.
struct Abort {
    path: String,
    reason: String,
}

enum JobClass {
    Delete(Vec<Delete>),
    Collected,
    Kept(String),
}

struct Pass<'a> {
    anchor: File,
    /// The home's device now: staged data of an earlier pass must be on it (a recorded
    /// device number need not survive a reboot or remount, its inode does).
    home_dev: u64,
    dry_run: bool,
    db: &'a Db,
    referenced: HashSet<Digest>,
    hook: &'a dyn Fn(CollectionStage, &Path) -> io::Result<()>,
}

/// Report accumulation.
struct Out {
    report: Report,
}
impl Out {
    fn push(&mut self, path: impl Into<String>, status: Status, reason: impl Into<String>) {
        self.report.entries.push(Entry {
            path: path.into(),
            status,
            reason: reason.into(),
        });
    }
    fn finish(mut self) -> Report {
        let mut summary = Summary::default();
        for e in &self.report.entries {
            *match e.status {
                Status::Candidate => &mut summary.candidate,
                Status::Deleted => &mut summary.deleted,
                Status::Collected => &mut summary.collected,
                Status::Retained => &mut summary.retained,
                Status::Refused => &mut summary.refused,
                Status::Skipped => &mut summary.skipped,
            } += 1;
        }
        self.report.summary = summary;
        self.report
            .entries
            .sort_by_key(|e| !matches!(e.status, Status::Refused | Status::Skipped));
        if self.report.entries.len() > REPORT_LIMIT {
            self.report.entries.truncate(REPORT_LIMIT);
            self.report.truncated = true;
        }
        self.report
    }
}

fn settled(rec: &EffectRecord, out: &ExecOutcome) -> bool {
    matches!(rec.state, EffectState::Completed | EffectState::Failed)
        && !out.unresolved
        && rec.result_digest.is_some()
        && out.receipt.effect_id == rec.effect_id
        && rec.result_digest == out.receipt.result_digest
        && rec.lease_generation == out.receipt.lease_generation
}

impl Pass<'_> {
    fn root_dir(&self, name: &str) -> Result<Option<File>, Fail> {
        owned_dir_opt(&self.anchor, name)
    }

    fn stage(&self, stage: CollectionStage, relative: &Path) -> Result<(), Fail> {
        (self.hook)(stage, relative)
            .map_err(|e| Fail::Refuse(format!("interrupted: {}", reason(e))))
    }

    fn scan(&self) -> Result<Scan, Abort> {
        let abort = |path: &str| {
            let path = path.to_string();
            move |f: Fail| Abort {
                path: path.clone(),
                reason: f.text().to_string(),
            }
        };
        let mut scan = Scan {
            tasks: BTreeMap::new(),
            tmp_tickets: Vec::new(),
        };
        fn add(tasks: &mut BTreeMap<String, (TaskId, Items)>, task: TaskId) -> &mut Items {
            &mut tasks
                .entry(task.as_str().to_string())
                .or_insert_with(|| (task, Items::default()))
                .1
        }
        let mut tmp_tickets = Vec::new();
        if let Some(trash) = self.root_dir("gc-trash").map_err(abort("gc-trash"))? {
            for entry in
                fs::read_dir(confined::path(&trash)).map_err(|e| abort("gc-trash")(e.into()))?
            {
                let entry = entry.map_err(|e| abort("gc-trash")(e.into()))?;
                let raw = entry.file_name();
                let shown = format!("gc-trash/{}", raw.to_string_lossy());
                let name = raw.to_str().ok_or_else(|| Abort {
                    path: shown.clone(),
                    reason: "unowned entry in gc-trash; inspect it manually".into(),
                })?;
                let ft = entry.file_type().map_err(|e| abort(&shown)(e.into()))?;
                let unowned = || Abort {
                    path: shown.clone(),
                    reason: "unowned entry in gc-trash; inspect it manually".into(),
                };
                if let Some(rest) = name.strip_prefix(TICKET_TMP_PREFIX) {
                    // An unpublished ticket of an interrupted pass: never executed.
                    let st = confined::stat(&trash, &raw)
                        .map_err(|e| abort(&shown)(e.into()))?
                        .ok_or_else(unowned)?;
                    if ft.is_file()
                        && st.st_nlink == 1
                        && rest.len() == 6
                        && rest.bytes().all(|b| b.is_ascii_alphanumeric())
                    {
                        tmp_tickets.push(name.to_string());
                        continue;
                    }
                    return Err(unowned());
                }
                if let Some(key) = name.strip_suffix(".json")
                    && hex_name(key)
                    && ft.is_file()
                {
                    let ticket = self.read_ticket(&trash, key).map_err(abort(&shown))?;
                    add(&mut scan.tasks, ticket.proof.task.clone())
                        .pending
                        .push((key.to_string(), ticket));
                    continue;
                }
                if hex_name(name)
                    && ft.is_dir()
                    && confined::stat(&trash, OsStr::new(&format!("{name}.json")))
                        .map_err(|e| abort(&shown)(e.into()))?
                        .is_some()
                {
                    continue;
                }
                return Err(unowned());
            }
        }
        for (sub, model) in [("jobs", false), ("model", true)] {
            let Some(root) = self.root_dir(sub).map_err(abort(sub))? else {
                continue;
            };
            for entry in fs::read_dir(confined::path(&root)).map_err(|e| abort(sub)(e.into()))? {
                let entry = entry.map_err(|e| abort(sub)(e.into()))?;
                let shown = format!("{sub}/{}", entry.file_name().to_string_lossy());
                let ft = entry.file_type().map_err(|e| abort(&shown)(e.into()))?;
                let name = entry.file_name().into_string().ok().filter(|_| ft.is_dir());
                let Some((name, effect)) =
                    name.and_then(|n| attempt_name(&n).map(|effect| (n, effect)))
                else {
                    return Err(Abort {
                        path: shown,
                        reason: "unknown name or not a directory (symlinks are refused)".into(),
                    });
                };
                let rec = self.db.effect(&effect).map_err(|e| Abort {
                    path: shown.clone(),
                    reason: format!("names an effect the journal does not know: {}", reason(e)),
                })?;
                if model && !matches!(rec.kind, EffectKind::ModelCall { .. }) {
                    return Err(Abort {
                        path: shown,
                        reason: "model retention for an effect that is not a model call".into(),
                    });
                }
                let items = add(&mut scan.tasks, rec.task_id);
                if model {
                    items.models.push(name);
                } else {
                    items.jobs.push(name);
                }
            }
        }
        if let Some(work) = self.root_dir("work").map_err(abort("work"))? {
            for entry in fs::read_dir(confined::path(&work)).map_err(|e| abort("work")(e.into()))? {
                let entry = entry.map_err(|e| abort("work")(e.into()))?;
                let shown = format!("work/{}", entry.file_name().to_string_lossy());
                let task = entry.file_name().to_str().and_then(task_id);
                let Some(task) = task else {
                    return Err(Abort {
                        path: shown,
                        reason: "unknown name in work".into(),
                    });
                };
                let dir = owned_dir(&work, task.as_str()).map_err(abort(&shown))?;
                let mut present = Vec::new();
                for name in WORKSPACES {
                    if confined::stat(&dir, OsStr::new(name))
                        .map_err(|e| abort(&shown)(e.into()))?
                        .is_some()
                    {
                        present.push(name);
                    }
                }
                // Parents left by earlier collection hold only `ws.lock`: streamed past.
                if !present.is_empty() {
                    self.db.task(&task).map_err(|e| Abort {
                        path: shown.clone(),
                        reason: format!(
                            "workspace of a task the journal does not know: {}",
                            reason(e)
                        ),
                    })?;
                    add(&mut scan.tasks, task).workspaces = present;
                }
            }
        }
        self.root_dir("inspect").map_err(abort("inspect"))?;
        scan.tmp_tickets = tmp_tickets;
        Ok(scan)
    }

    fn read_ticket(&self, trash: &File, key: &str) -> Result<Ticket, Fail> {
        let value: serde_json::Value =
            read_json_at(trash, &format!("{key}.json")).map_err(Fail::Integrity)?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version != Some(TICKET_VERSION) {
            return Err(Fail::Integrity(format!(
                "deletion ticket version {version:?} is not executed by this build \
                 (version {TICKET_VERSION}); inspect gc-trash manually"
            )));
        }
        let ticket: Ticket = serde_json::from_value(value).map_err(|e| {
            Fail::Integrity(format!(
                "malformed deletion ticket: {:?} error",
                e.classify()
            ))
        })?;
        if key_of(&ticket.relative) != key {
            return Err(Fail::Integrity("deletion ticket path pin mismatch".into()));
        }
        ticket_shape(&ticket).map_err(Fail::Integrity)?;
        Ok(ticket)
    }

    /// Task-level conditions: everything of the task is settled and nothing inspects it.
    fn check_task(&self, task: &TaskId) -> Result<(), Fail> {
        let t = self
            .db
            .task(task)
            .map_err(|e| Fail::Integrity(format!("task is not in the journal: {}", reason(e))))?;
        // A terminal task accepts no further event, so a cancellation still marked pending
        // (a task that failed while it was requested) can no longer start anything.
        if !t.state.is_terminal() {
            return Err(Fail::Retain(format!(
                "task is not finished ({:?})",
                t.state
            )));
        }
        let outstanding = self
            .db
            .outstanding_effects(task)
            .map_err(|e| Fail::Refuse(reason(e)))?;
        if !outstanding.is_empty() {
            return Err(Fail::Retain(format!(
                "task has {} unsettled effect(s)",
                outstanding.len()
            )));
        }
        if let Some(inspect) = self.root_dir("inspect")?
            && let Some(dir) = owned_dir_opt(&inspect, task.as_str())?
            && fs::read_dir(confined::path(&dir))?.next().is_some()
        {
            return Err(Fail::Retain("inspection leftovers are present".into()));
        }
        Ok(())
    }

    /// Takes `work/<task>/ws.lock` (creating it, as workers do) when the parent exists.
    fn workspace_lock(&self, task: &TaskId) -> Result<Option<WsLock>, Fail> {
        let Some(work) = self.root_dir("work")? else {
            return Ok(None);
        };
        let Some(dir) = owned_dir_opt(&work, task.as_str())? else {
            return Ok(None);
        };
        // A dry run creates nothing: a missing lock file is one nobody holds.
        let create = if self.dry_run {
            OFlags::empty()
        } else {
            OFlags::CREATE
        };
        let lock: File = match openat(
            &dir,
            "ws.lock",
            OFlags::RDWR | create | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        ) {
            Ok(fd) => fd.into(),
            Err(rustix::io::Errno::NOENT) if self.dry_run => return Ok(None),
            Err(rustix::io::Errno::LOOP) => {
                return Err(Fail::Integrity("ws.lock is a symlink".into()));
            }
            Err(e) => return Err(e.into()),
        };
        let meta = lock.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(Fail::Retain(
                "ws.lock is not a single-link regular file".into(),
            ));
        }
        if !try_lock_briefly(&lock) {
            return Err(Fail::Retain("the workspace lock is held".into()));
        }
        Ok(Some((lock, confined::dir_identity(&dir)?)))
    }

    /// Takes a job's lock: busy means the job may still run. A missing lock file means
    /// nobody holds it (as `JobDir::lock_held`).
    fn job_lock(&self, job: &File) -> Result<Option<File>, Fail> {
        let lock: File = match openat(
            job,
            "lock",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd.into(),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = lock.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(Fail::Retain(
                "job lock is not a single-link regular file".into(),
            ));
        }
        if !try_lock_briefly(&lock) {
            return Err(Fail::Retain("the job is live (its lock is held)".into()));
        }
        Ok(Some(lock))
    }

    fn classify_job(&self, task: &TaskId, name: &str) -> Result<JobClass, Fail> {
        let jobs = self
            .root_dir("jobs")?
            .ok_or_else(|| Fail::Refuse("jobs directory vanished".into()))?;
        let job = owned_dir(&jobs, name)?;
        let job_identity = confined::dir_identity(&job)?;
        if confined::stat(&job, OsStr::new("jail"))?.is_some() {
            return Err(Fail::Retain("jail leftovers of the job are present".into()));
        }
        let request: Result<JobRequest, String> = read_json_at(&job, "request.json");
        let firecracker =
            matches!(&request, Ok(r) if matches!(r.worker, WorkerConfig::Firecracker(_)));
        check_tree(
            &pinned(&job),
            Rules {
                socket_root: false,
                socket_child: firecracker.then_some("v.sock"),
            },
        )?;
        drop(self.job_lock(&job)?);
        if confined::stat(&job, OsStr::new("receipt.json"))?.is_none() {
            return Ok(JobClass::Kept(
                "no receipt (an unfinished or killed attempt); its files are kept".into(),
            ));
        }
        let kept = |why: String| -> Result<JobClass, Fail> {
            Ok(JobClass::Kept(format!(
                "{why}; the attempt's files are kept"
            )))
        };
        let out: ExecOutcome = match read_json_at(&job, "receipt.json") {
            Ok(out) => out,
            Err(why) => return kept(why),
        };
        let req = match request {
            Ok(req) => req,
            Err(why) => return kept(why),
        };
        let Ok(rec) = self.db.effect(&out.receipt.effect_id) else {
            return kept("the receipt names an unknown effect".into());
        };
        if !settled(&rec, &out)
            || rec.task_id != *task
            || req.effect_id != rec.effect_id
            || req.task_id != rec.task_id
            || req.kind != rec.kind
            || req.attempt_id != out.receipt.attempt_id
            || req.lease_generation != out.receipt.lease_generation
            || name != format!("{}-{}", rec.effect_id, out.receipt.attempt_id)
        {
            return kept(
                "the receipt is not the settled published result (superseded or unresolved attempt)"
                    .into(),
            );
        }
        let digest = rec.result_digest.expect("settled checks the digest");
        if !self.referenced.contains(&digest) {
            return kept("the result is not a registered artifact".into());
        }
        let mut deletes = Vec::new();
        for file in JOB_FILES {
            let Some(st) = confined::stat(&job, OsStr::new(file))? else {
                continue;
            };
            let ft = confined::file_type(&st);
            let fits =
                ft == expected_type(Kind::JobFile, file) && (file != "v.sock" || firecracker);
            if !fits {
                return Err(Fail::Retain(format!("unexpected {file} in the job")));
            }
            if file == "output.bin" {
                let bytes = read_file_at(&job, file).map_err(Fail::Retain)?;
                if Digest::of(&bytes) != digest {
                    return kept("output.bin does not match the published result".into());
                }
            }
            deletes.push(Delete {
                relative: Path::new("jobs").join(name).join(file),
                kind: Kind::JobFile,
                proof: Proof {
                    task: task.clone(),
                    effect: Some(rec.effect_id.clone()),
                    attempt: Some(out.receipt.attempt_id.clone()),
                    result: Some(digest),
                    lease: rec.lease_generation,
                    firecracker_socket: firecracker,
                },
                identity: confined::identity_of(&st),
                file_type: ft,
                parent: job_identity,
            });
        }
        Ok(if deletes.is_empty() {
            JobClass::Collected
        } else {
            JobClass::Delete(deletes)
        })
    }

    fn classify_model(&self, task: &TaskId, name: &str) -> Result<Result<Delete, String>, Fail> {
        let root = self
            .root_dir("model")?
            .ok_or_else(|| Fail::Refuse("model directory vanished".into()))?;
        let dir = owned_dir(&root, name)?;
        let identity = confined::dir_identity(&dir)?;
        check_tree(&pinned(&dir), Rules::default())?;
        let out: ExecOutcome = match read_json_at(&dir, "response.json") {
            Ok(out) => out,
            Err(why) => return Ok(Err(why)),
        };
        let Ok(rec) = self.db.effect(&out.receipt.effect_id) else {
            return Ok(Err("the response names an unknown effect".into()));
        };
        if !settled(&rec, &out)
            || rec.task_id != *task
            || !matches!(rec.kind, EffectKind::ModelCall { .. })
            || name != format!("{}-{}", rec.effect_id, out.receipt.attempt_id)
            || rec.result_digest != Some(Digest::of(&out.output))
            || !rec
                .result_digest
                .is_some_and(|d| self.referenced.contains(&d))
        {
            return Ok(Err(
                "the response is not the settled published result".into()
            ));
        }
        Ok(Ok(Delete {
            relative: Path::new("model").join(name),
            kind: Kind::Model,
            proof: Proof {
                task: task.clone(),
                effect: Some(rec.effect_id.clone()),
                attempt: Some(out.receipt.attempt_id.clone()),
                result: rec.result_digest,
                lease: rec.lease_generation,
                firecracker_socket: false,
            },
            identity,
            file_type: FileType::Directory,
            parent: confined::dir_identity(&root)?,
        }))
    }

    fn classify_workspace(
        &self,
        task: &TaskId,
        name: &'static str,
    ) -> Result<Option<Delete>, Fail> {
        let Some(work) = self.root_dir("work")? else {
            return Ok(None);
        };
        let Some(dir) = owned_dir_opt(&work, task.as_str())? else {
            return Ok(None);
        };
        let Some(st) = confined::stat(&dir, OsStr::new(name))? else {
            return Ok(None);
        };
        let ft = confined::file_type(&st);
        if ft == FileType::Symlink {
            return Err(Fail::Integrity(format!("work/{task}/{name} is a symlink")));
        }
        // Workers write `ws`/`workspace` as directories and `ws.img` as a file; staging any
        // other shape would leave data a later pass cannot recognise as its own.
        if ft != expected_type(Kind::Workspace, name) {
            return Err(Fail::Retain(format!(
                "work/{task}/{name} is not a {}; kept",
                if name == "ws.img" {
                    "regular file"
                } else {
                    "directory"
                }
            )));
        }
        check_tree(&confined::path(&dir).join(name), Rules::default())?;
        Ok(Some(Delete {
            relative: Path::new("work").join(task.as_str()).join(name),
            kind: Kind::Workspace,
            proof: Proof {
                task: task.clone(),
                effect: None,
                attempt: None,
                result: None,
                lease: 0,
                firecracker_socket: false,
            },
            identity: confined::identity_of(&st),
            file_type: ft,
            parent: confined::dir_identity(&dir)?,
        }))
    }

    /// The staged deletion's proof still holds against the journal and registered blobs.
    fn validate_proof(&self, ticket: &Ticket) -> Result<(), Fail> {
        let Some(effect) = &ticket.proof.effect else {
            return Ok(());
        };
        let rec = self.db.effect(effect).map_err(|e| {
            Fail::Integrity(format!(
                "staged deletion names an unknown effect: {}",
                reason(e)
            ))
        })?;
        let result = ticket.proof.result;
        if rec.task_id != ticket.proof.task
            || !matches!(rec.state, EffectState::Completed | EffectState::Failed)
            || rec.result_digest != result
            || rec.lease_generation != ticket.proof.lease
            || !result.is_some_and(|d| self.referenced.contains(&d))
            || (ticket.kind == Kind::Model && !matches!(rec.kind, EffectKind::ModelCall { .. }))
        {
            return Err(Fail::Integrity(
                "staged deletion is no longer proven by a settled published effect".into(),
            ));
        }
        Ok(())
    }

    fn classify_pending(&self, key: &str, ticket: &Ticket) -> Result<Action, Fail> {
        let trash = self
            .root_dir("gc-trash")?
            .ok_or_else(|| Fail::Refuse("gc-trash vanished".into()))?;
        let data = match owned_dir_opt(&trash, key)? {
            Some(stage) => confined::stat(&stage, OsStr::new("data"))?.map(|st| (stage, st)),
            None => None,
        };
        let source = match confined::parent(&self.anchor, &ticket.relative) {
            Ok(parent) => {
                confined::stat(&parent, ticket.relative.file_name().expect("shape checked"))?
                    .is_some()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(path_error(e)),
        };
        let key = key.to_string();
        let ticket = ticket.clone();
        Ok(match data {
            Some((stage, st)) => {
                if !self.staged_by(&ticket, &st) {
                    // Not provably what this ticket staged: never removed, never moved.
                    return Err(Fail::Integrity(format!(
                        "gc-trash/{key}/data is not the entry its ticket staged; inspect it manually"
                    )));
                }
                if ticket.restore {
                    return Ok(Action::Restore { key, ticket });
                }
                self.validate_proof(&ticket)?;
                match check_tree(&confined::path(&stage).join("data"), ticket.rules()) {
                    Ok(()) => Action::Finish { key, ticket },
                    Err(Fail::Transient(why)) => return Err(Fail::Transient(why)),
                    // Ours, but no longer safe to remove blindly: move it back.
                    Err(_) => Action::Restore { key, ticket },
                }
            }
            None if source => Action::Retire { key },
            None => Action::Finish { key, ticket },
        })
    }

    /// Whether `st` (staged by an earlier pass) is the entry `ticket` staged: the same inode
    /// and kind of file, on the home's current device. The recorded device number is not
    /// compared across passes: it need not survive a reboot or remount.
    fn staged_by(&self, ticket: &Ticket, st: &rustix::fs::Stat) -> bool {
        st.st_ino == ticket.inode
            && st.st_dev == self.home_dev
            && ticket_accepts(ticket, confined::file_type(st))
    }

    /// Classifies one task without deleting anything. `Err` is an integrity problem.
    fn classify(&self, task: &TaskId, items: &Items) -> Result<TaskPlan, Abort> {
        let mut plan = TaskPlan {
            lock: None,
            blocked: None,
            actions: Vec::new(),
            notes: Vec::new(),
        };
        // Retain the task at `path`; an integrity problem stops the pass.
        let block = |plan: &mut TaskPlan, path: String, fail: Fail| match fail {
            Fail::Integrity(reason) => Err(Abort { path, reason }),
            fail => {
                plan.blocked = Some((path, fail));
                Ok(())
            }
        };
        let task_path = format!("task {task}");
        if let Err(fail) = self.check_task(task) {
            block(&mut plan, task_path, fail)?;
            return Ok(plan);
        }
        match self.workspace_lock(task) {
            Ok(lock) => plan.lock = lock,
            Err(fail) => {
                block(&mut plan, format!("work/{task}/ws.lock"), fail)?;
                return Ok(plan);
            }
        }
        for (key, ticket) in &items.pending {
            match self.classify_pending(key, ticket) {
                Ok(action) => plan.actions.push(action),
                Err(fail) => {
                    block(&mut plan, format!("gc-trash/{key}"), fail)?;
                    return Ok(plan);
                }
            }
        }
        for name in &items.jobs {
            let path = format!("jobs/{name}");
            match self.classify_job(task, name) {
                Ok(JobClass::Delete(deletes)) => {
                    plan.actions.extend(deletes.into_iter().map(Action::Delete))
                }
                Ok(JobClass::Collected) => plan.notes.push(Entry {
                    path,
                    status: Status::Collected,
                    reason: "redundant copies already removed; logs, status and receipt kept"
                        .into(),
                }),
                Ok(JobClass::Kept(why)) => plan.notes.push(Entry {
                    path,
                    status: Status::Retained,
                    reason: why,
                }),
                Err(fail) => {
                    block(&mut plan, path, fail)?;
                    return Ok(plan);
                }
            }
        }
        for name in &items.models {
            let path = format!("model/{name}");
            match self.classify_model(task, name) {
                Ok(Ok(delete)) => plan.actions.push(Action::Delete(delete)),
                Ok(Err(why)) => plan.notes.push(Entry {
                    path,
                    status: Status::Retained,
                    reason: format!("{why}; kept"),
                }),
                Err(fail) => {
                    block(&mut plan, path, fail)?;
                    return Ok(plan);
                }
            }
        }
        for name in &items.workspaces {
            let path = format!("work/{task}/{name}");
            match self.classify_workspace(task, name) {
                Ok(Some(delete)) => plan.actions.push(Action::Delete(delete)),
                Ok(None) => {}
                Err(fail) => {
                    block(&mut plan, path, fail)?;
                    return Ok(plan);
                }
            }
        }
        Ok(plan)
    }

    fn report_blocked(out: &mut Out, task: &TaskId, items: &Items, plan: &TaskPlan) {
        let Some((at, fail)) = &plan.blocked else {
            return;
        };
        let refused = matches!(fail, Fail::Refuse(_) | Fail::Transient(_));
        let mut seen = false;
        for path in items.paths(task) {
            let own = path == *at || at.starts_with(&format!("{path}/"));
            seen |= own;
            let status = if refused && own {
                Status::Refused
            } else {
                Status::Retained
            };
            out.push(
                path,
                status,
                format!("task retained: {at}: {}", fail.text()),
            );
        }
        if refused && !seen {
            out.push(at.clone(), Status::Refused, fail.text().to_string());
        }
    }

    fn execute(&self, plan: &TaskPlan, action: &Action) -> Result<(Status, String), Fail> {
        match action {
            Action::Delete(d) => {
                self.delete(plan, d)?;
                Ok((Status::Deleted, action.planned().into()))
            }
            Action::Finish { key, ticket } => {
                self.validate_proof(ticket)?;
                let trash = self.trash()?;
                let stage = owned_dir_opt(&trash, key)?;
                self.finish_staged(&trash, key, stage.as_ref(), ticket, None)?;
                Ok((
                    Status::Deleted,
                    "interrupted deletion of a proven copy finished".into(),
                ))
            }
            Action::Retire { key } => {
                self.retire(&self.trash()?, key)?;
                Ok((
                    Status::Retained,
                    "unexecuted deletion ticket dropped; data kept".into(),
                ))
            }
            Action::Restore { key, ticket } => {
                let trash = self.trash()?;
                let stage = owned_dir(&trash, key)?;
                // Reported as a refusal either way: something changed staged data.
                let staged = confined::stat(&stage, OsStr::new("data"))?
                    .ok_or_else(|| Fail::Refuse("staged data vanished".into()))?;
                Err(self.put_back(
                    &trash,
                    key,
                    &stage,
                    ticket,
                    None,
                    Fail::Refuse(if ticket.restore {
                        "staged data changed after validation".into()
                    } else {
                        "staged data no longer passes its checks".into()
                    }),
                    &staged,
                ))
            }
        }
    }

    fn trash(&self) -> Result<File, Fail> {
        Ok(confined::directory(&self.anchor, OsStr::new("gc-trash"))?)
    }

    fn retire(&self, trash: &File, key: &str) -> Result<(), Fail> {
        // The staging directory goes first: a non-empty one keeps its ticket.
        confined::unlink_if_present(trash, OsStr::new(key), true)?;
        confined::unlink_if_present(trash, OsStr::new(&format!("{key}.json")), false)?;
        Ok(())
    }

    /// Moves staged data back to its original place and drops the ticket. Returns the
    /// failure to report: `fail` (as a refusal unless it is an integrity problem) when the
    /// data is back, an integrity problem when it could not be moved back.
    ///
    /// A move back that fails for lack of resources leaves the data staged under a ticket
    /// that a later pass honours: the ticket already names this inode, or it is rewritten
    /// to name it with `restore` set (data that changed after validation is never removed).
    #[allow(clippy::too_many_arguments)]
    fn put_back(
        &self,
        trash: &File,
        key: &str,
        stage: &File,
        ticket: &Ticket,
        parent: Option<&File>,
        fail: Fail,
        staged: &rustix::fs::Stat,
    ) -> Fail {
        let name = ticket.relative.file_name().expect("shape checked");
        let moved =
            (self.hook)(CollectionStage::Restoring, &ticket.relative).and_then(|()| match parent {
                Some(p) => confined::restore(stage, p, name),
                None => confined::parent(&self.anchor, &ticket.relative)
                    .and_then(|p| confined::restore(stage, &p, name)),
            });
        let moved = match moved {
            Ok(()) => Ok(()),
            Err(e) => Err(Fail::from(e)),
        };
        match moved {
            Ok(()) => {
                let kept = match &fail {
                    Fail::Integrity(why) => format!("{why}; moved back and kept"),
                    other => format!("{}; moved back and kept", other.text()),
                };
                match self.retire(trash, key) {
                    Ok(()) if matches!(fail, Fail::Integrity(_)) => Fail::Integrity(kept),
                    Ok(()) => Fail::Refuse(kept),
                    // The data is back; the leftover ticket names an entry that never moved
                    // again, so the next pass drops it.
                    Err(e) => Fail::Refuse(format!(
                        "{kept}; dropping its ticket failed ({}), the next pass drops it",
                        e.text()
                    )),
                }
            }
            Err(Fail::Transient(why)) => {
                // A ticket naming this very inode already leads the next pass to move it back
                // (its checks fail again) or to finish it (they pass): no rewrite needed. A
                // ticket from an earlier pass is matched as later passes match it.
                let named = match parent {
                    Some(_) => (ticket.device, ticket.inode) == confined::identity_of(staged),
                    None => self.staged_by(ticket, staged),
                };
                // Only promise a later move back if the later reader accepts what is written.
                let rewritten = restore_ticket(
                    ticket,
                    confined::identity_of(staged),
                    confined::file_type(staged),
                );
                let marked = named
                    || (ticket_accepts(&rewritten, confined::file_type(staged))
                        && publish_ticket(trash, key, &rewritten, true).is_ok());
                if marked {
                    Fail::Transient(format!(
                        "{}; moving it back failed ({why}); it stays staged and the next pass \
                         moves it back",
                        fail.text()
                    ))
                } else {
                    Fail::Integrity(format!(
                        "{}; the staged data could not be moved back ({why}); inspect gc-trash/{key}",
                        fail.text()
                    ))
                }
            }
            Err(e) => Fail::Integrity(format!(
                "{}; the staged data could not be moved back ({}); inspect gc-trash/{key}",
                fail.text(),
                e.text()
            )),
        }
    }

    fn delete(&self, plan: &TaskPlan, d: &Delete) -> Result<(), Fail> {
        let parent = confined::parent(&self.anchor, &d.relative).map_err(path_error)?;
        if confined::dir_identity(&parent)? != d.parent
            || (d.kind == Kind::Workspace && plan.lock.as_ref().map(|l| l.1) != Some(d.parent))
        {
            return Err(Fail::Refuse(
                "parent directory changed after validation; kept".into(),
            ));
        }
        let name = d.relative.file_name().expect("owned name");
        let _job_lock = if d.kind == Kind::JobFile {
            self.job_lock(&parent)
                .map_err(|f| Fail::Refuse(f.text().to_string()))?
        } else {
            None
        };
        let trash = self.trash()?;
        let key = key_of(&d.relative);
        if confined::stat(&trash, OsStr::new(&format!("{key}.json")))?.is_some() {
            return Err(Fail::Refuse(
                "a deletion ticket for this path already exists".into(),
            ));
        }
        let ticket = removal_ticket(d);
        if !ticket_accepts(&ticket, d.file_type) {
            // Classification stages only expected kinds; never write what no pass can read.
            return Err(Fail::Refuse(
                "unexpected kind of file for this entry; kept".into(),
            ));
        }
        publish_ticket(&trash, &key, &ticket, false)?;
        self.stage(CollectionStage::TicketPublished, &d.relative)?;
        let stage = confined::directory(&trash, OsStr::new(&key))?;
        if let Err(e) = confined::move_entry(&parent, name, &stage) {
            // Nothing moved (or the move is not durable yet): drop the unexecuted ticket
            // when the staging directory is still empty; otherwise a rerun finishes it.
            let _ = self.retire(&trash, &key);
            return Err(Fail::Refuse(format!("cannot stage: {}; kept", reason(e))));
        }
        self.finish_staged(&trash, &key, Some(&stage), &ticket, Some(&parent))
    }

    /// Removes staged data proven by `ticket`, then its staging directory, then the ticket.
    /// Data that is not what the ticket proved is moved back instead.
    fn finish_staged(
        &self,
        trash: &File,
        key: &str,
        stage: Option<&File>,
        ticket: &Ticket,
        parent: Option<&File>,
    ) -> Result<(), Fail> {
        if let Some(stage) = stage
            && let Some(st) = confined::stat(stage, OsStr::new("data"))?
        {
            let same = match parent {
                // Moved by this very call: same boot and mount, so device and inode.
                Some(_) => confined::identity_of(&st) == (ticket.device, ticket.inode),
                None => self.staged_by(ticket, &st),
            };
            let checked = if !same {
                if parent.is_none() {
                    // Staged by an earlier pass: not provably ours, so never moved.
                    return Err(Fail::Integrity(format!(
                        "gc-trash/{key}/data is not the entry its ticket staged; inspect it manually"
                    )));
                }
                // Moved by this very call: undo the move.
                Err(Fail::Refuse("the entry changed after validation".into()))
            } else {
                check_tree(&confined::path(stage).join("data"), ticket.rules())
            };
            match checked {
                Ok(()) => {}
                // The data is the validated entry and its ticket holds: the next pass checks
                // and finishes it.
                Err(Fail::Transient(why)) if same => {
                    return Err(Fail::Transient(format!(
                        "{why}; staged, finished by the next pass"
                    )));
                }
                Err(fail) => {
                    return Err(self.put_back(trash, key, stage, ticket, parent, fail, &st));
                }
            }
            self.stage(CollectionStage::Staged, &ticket.relative)?;
            let mut remaining = TREE_LIMIT;
            confined::remove(stage, OsStr::new("data"), &mut remaining)?;
        }
        // The ticket outlives payload and staging directory removal: an interruption
        // leaves enough ownership proof to retry without the original receipt files.
        confined::unlink_if_present(trash, OsStr::new(key), true)?;
        self.stage(CollectionStage::Removed, &ticket.relative)?;
        confined::unlink_if_present(trash, OsStr::new(&format!("{key}.json")), false)?;
        Ok(())
    }
}

/// Takes an exclusive lock without waiting for a real holder. A process forked by another
/// thread shares the open file description of a lock this pass just released until it
/// execs, so a busy answer is retried for a moment before it counts as held.
fn try_lock_briefly(lock: &File) -> bool {
    for attempt in 0..5 {
        if lock.try_lock().is_ok() {
            return true;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    false
}

/// The database and blob store handed to `collect` are this home's (`agentos.db` and
/// `blobs/objects` under the anchored directory, compared by device and inode). A symlinked
/// `agentos.db` or `blobs` resolves elsewhere and is refused too.
fn belongs_to_home(anchor: &File, db: &Db, blobs: &BlobStore) -> Result<(), String> {
    let same = |relative: &[&str], path: Option<&Path>| -> bool {
        let Some(path) = path else { return false };
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let mut dir = match anchor.try_clone() {
            Ok(dir) => dir,
            Err(_) => return false,
        };
        let (last, parents) = relative.split_last().expect("non-empty");
        for name in parents {
            match confined::open_dir(&dir, OsStr::new(name)) {
                Ok(next) => dir = next,
                Err(_) => return false,
            }
        }
        matches!(confined::stat(&dir, OsStr::new(last)), Ok(Some(st))
            if confined::identity_of(&st) == (meta.dev(), meta.ino()))
    };
    if !same(&["agentos.db"], db.path()) {
        return Err(
            "the database is not this home's agentos.db (an in-memory or symlinked database \
             is refused)"
                .into(),
        );
    }
    if !same(&["blobs", "objects"], Some(blobs.objects_dir())) {
        return Err(
            "the blob store is not this home's blobs (a symlinked blobs directory is refused)"
                .into(),
        );
    }
    Ok(())
}

/// Publishes `ticket` durably; `replace` overwrites an existing ticket of the same key
/// atomically (only to mark staged data for moving back).
fn publish_ticket(trash: &File, key: &str, ticket: &Ticket, replace: bool) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = tempfile::Builder::new()
        .prefix(TICKET_TMP_PREFIX)
        .rand_bytes(6)
        .tempfile_in(confined::path(trash))?;
    tmp.write_all(&serde_json::to_vec(ticket).map_err(io::Error::other)?)?;
    tmp.as_file().sync_all()?;
    let path = confined::path(trash).join(format!("{key}.json"));
    if replace {
        tmp.persist(path).map_err(|e| e.error)?;
    } else {
        tmp.persist_noclobber(path).map_err(|e| e.error)?;
    }
    trash.sync_all()
}

/// Collects settled redundant copies of the home `lock` proves is held.
pub fn collect(
    lock: &HeldDriverLock<'_>,
    db: &Db,
    blobs: &BlobStore,
    opts: Options,
) -> io::Result<Report> {
    collect_with_hook(lock, db, blobs, opts, &|_, _| Ok(()))
}

/// [`collect`] with a hook at each [`CollectionStage`]; an error from it interrupts the
/// pass at that point, as a crash would.
pub fn collect_with_hook(
    lock: &HeldDriverLock<'_>,
    db: &Db,
    blobs: &BlobStore,
    opts: Options,
    hook: &dyn Fn(CollectionStage, &Path) -> io::Result<()>,
) -> io::Result<Report> {
    let batch_size = opts.batch_size.max(1);
    let task_cap = tasks_per_batch();
    let mut out = Out {
        report: Report {
            dry_run: opts.dry_run,
            batch_size,
            tasks_per_batch: task_cap,
            ..Report::default()
        },
    };
    // The directory the driver lock was proven through: never reopened by path.
    let anchor = lock.anchor.try_clone()?;
    let home_dev = rustix::fs::fstat(&anchor)?.st_dev;
    if let Err(why) = belongs_to_home(&anchor, db, blobs) {
        out.push(".", Status::Refused, why);
        return Ok(out.finish());
    }
    let referenced = db.referenced_blobs().map_err(io::Error::other)?;
    for digest in &referenced {
        if let Err(e) = blobs.get(digest) {
            out.push(
                "blobs",
                Status::Refused,
                format!("a registered blob is unreadable or corrupt: {}", reason(&e)),
            );
            return Ok(out.finish());
        }
    }
    let pass = Pass {
        anchor,
        home_dev,
        dry_run: opts.dry_run,
        db,
        referenced,
        hook,
    };
    let scan = match pass.scan() {
        Ok(scan) => scan,
        Err(abort) => {
            out.push(abort.path, Status::Refused, abort.reason);
            return Ok(out.finish());
        }
    };
    let stop = |out: &mut Out, from: usize, why: &str| {
        for (task, items) in scan.tasks.values().skip(from) {
            for path in items.paths(task) {
                out.push(path, Status::Skipped, why.to_string());
            }
        }
    };
    // Phase 1: classify everything; nothing is deleted and no descriptor is kept.
    let mut eligible = Vec::new();
    for (i, (task, items)) in scan.tasks.values().enumerate() {
        match pass.classify(task, items) {
            Ok(plan) => {
                if plan.blocked.is_some() {
                    Pass::report_blocked(&mut out, task, items, &plan);
                } else if plan.actions.is_empty() || opts.dry_run {
                    for note in plan.notes {
                        out.report.entries.push(note);
                    }
                    for action in &plan.actions {
                        out.push(action.path(), Status::Candidate, action.planned());
                    }
                } else {
                    eligible.push((i, plan.actions.len()));
                }
            }
            Err(abort) => {
                let why = format!("pass stopped at {}", abort.path);
                out.report.entries.clear();
                out.push(abort.path, Status::Refused, abort.reason);
                stop(&mut out, 0, &why);
                return Ok(out.finish());
            }
        }
    }
    for name in &scan.tmp_tickets {
        let path = format!("gc-trash/{name}");
        let why = "unpublished deletion ticket of an interrupted pass";
        if opts.dry_run {
            out.push(path, Status::Candidate, why);
            continue;
        }
        match pass
            .trash()
            .and_then(|t| Ok(confined::unlink_if_present(&t, OsStr::new(name), false)?))
        {
            Ok(()) => out.push(path, Status::Deleted, why),
            Err(fail) => out.push(path, Status::Refused, fail.text().to_string()),
        }
    }
    if opts.dry_run {
        return Ok(out.finish());
    }
    // Phase 2: batches of whole tasks are revalidated (holding their workspace locks),
    // then deleted entry by entry.
    let tasks: Vec<&(TaskId, Items)> = scan.tasks.values().collect();
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut size = 0;
    for (i, n) in eligible {
        // A batch holds one `ws.lock` per task: the task count is capped whatever the
        // entry-based batch size says.
        if batches.is_empty()
            || size >= batch_size
            || batches.last().is_some_and(|b| b.len() >= task_cap)
        {
            batches.push(Vec::new());
            size = 0;
        }
        batches.last_mut().expect("pushed").push(i);
        size += n;
    }
    let skip_batches = |out: &mut Out, batches: &[Vec<usize>], why: &str| {
        for &i in batches.iter().flatten() {
            let (task, items) = tasks[i];
            for path in items.paths(task) {
                out.push(path, Status::Skipped, why.to_string());
            }
        }
    };
    for (b, batch) in batches.iter().enumerate() {
        out.report.batches += 1;
        let mut prepared = Vec::new();
        for &i in batch {
            let (task, items) = tasks[i];
            match pass.classify(task, items) {
                Ok(plan) if plan.blocked.is_some() => {
                    Pass::report_blocked(&mut out, task, items, &plan)
                }
                Ok(plan) => prepared.push(plan),
                Err(abort) => {
                    let why = format!("pass stopped at {}", abort.path);
                    out.push(abort.path, Status::Refused, abort.reason);
                    for plan in &prepared {
                        for action in &plan.actions {
                            out.push(action.path(), Status::Skipped, why.clone());
                        }
                    }
                    skip_batches(&mut out, &batches[b + 1..], &why);
                    return Ok(out.finish());
                }
            }
        }
        if let Err(e) = (pass.hook)(CollectionStage::Validated, Path::new(".")) {
            let why = "pass interrupted after validation";
            out.push(".", Status::Refused, format!("{why}: {}", reason(e)));
            for plan in &prepared {
                for action in &plan.actions {
                    out.push(action.path(), Status::Skipped, why);
                }
            }
            skip_batches(&mut out, &batches[b + 1..], why);
            return Ok(out.finish());
        }
        let mut abort: Option<String> = None;
        for plan in &prepared {
            for note in &plan.notes {
                out.push(note.path.clone(), note.status, note.reason.clone());
            }
            let mut stopped: Option<String> = abort.clone();
            for action in &plan.actions {
                let path = action.path();
                if let Some(why) = &stopped {
                    out.push(path, Status::Skipped, why.clone());
                    continue;
                }
                match pass.execute(plan, action) {
                    Ok((status, why)) => out.push(path, status, why),
                    Err(Fail::Integrity(why)) => {
                        let stop_why = format!("pass stopped at {path}");
                        out.push(path, Status::Refused, why);
                        abort = Some(stop_why.clone());
                        stopped = Some(stop_why);
                    }
                    Err(fail) => {
                        stopped = Some(format!("task stopped after a failure at {path}"));
                        out.push(path, Status::Refused, fail.text().to_string());
                    }
                }
            }
        }
        drop(prepared);
        if let Some(why) = abort {
            skip_batches(&mut out, &batches[b + 1..], &why);
            return Ok(out.finish());
        }
    }
    Ok(out.finish())
}

#[cfg(test)]
mod ticket_tests {
    use super::*;

    const ALL: [FileType; 8] = [
        FileType::Directory,
        FileType::RegularFile,
        FileType::Socket,
        FileType::Symlink,
        FileType::Fifo,
        FileType::CharacterDevice,
        FileType::BlockDevice,
        FileType::Unknown,
    ];

    fn entries() -> Vec<(Kind, String, bool)> {
        let effect = "e".repeat(64);
        let attempt = "00000000-0000-0000-0000-000000000000";
        let task = "11111111-1111-1111-1111-111111111111";
        let mut out = Vec::new();
        for name in WORKSPACES {
            out.push((Kind::Workspace, format!("work/{task}/{name}"), false));
        }
        for name in JOB_FILES {
            out.push((
                Kind::JobFile,
                format!("jobs/{effect}-{attempt}/{name}"),
                true,
            ));
        }
        out.push((Kind::Model, format!("model/{effect}-{attempt}"), false));
        out
    }

    fn ticket(kind: Kind, relative: &str, firecracker: bool) -> Ticket {
        let has_effect = kind != Kind::Workspace;
        let ticket = Ticket {
            version: TICKET_VERSION,
            relative: relative.into(),
            kind,
            proof: Proof {
                task: serde_json::from_value(serde_json::json!(
                    "11111111-1111-1111-1111-111111111111"
                ))
                .unwrap(),
                effect: has_effect
                    .then(|| serde_json::from_value(serde_json::json!("e".repeat(64))).unwrap()),
                attempt: has_effect.then(|| {
                    serde_json::from_value(serde_json::json!(
                        "00000000-0000-0000-0000-000000000000"
                    ))
                    .unwrap()
                }),
                result: has_effect.then(|| Digest::of(b"r")),
                lease: 1,
                firecracker_socket: firecracker,
            },
            device: 1,
            inode: 2,
            restore: false,
            file_type: None,
        };
        assert_eq!(ticket_shape(&ticket), Ok(()), "{relative}");
        ticket
    }

    /// Every ticket a writer can produce is accepted by the reader (write-set within
    /// read-set), and a removal ticket never accepts another kind of file.
    #[test]
    fn every_written_ticket_is_read_back_and_removal_stays_strict() {
        for (kind, relative, firecracker) in entries() {
            let base = ticket(kind, &relative, firecracker);
            let name = relative.rsplit('/').next().unwrap();
            let expected = expected_type(kind, name);
            for ft in ALL {
                // Removal: classification stages only the expected kind (it checks
                // `expected_type`); that ticket must be readable, any other must not be.
                let delete = Delete {
                    relative: relative.clone().into(),
                    kind,
                    proof: base.proof.clone(),
                    identity: (1, 2),
                    file_type: ft,
                    parent: (1, 3),
                };
                let removal = removal_ticket(&delete);
                assert_eq!(
                    ticket_accepts(&removal, ft),
                    ft == expected,
                    "{relative} {ft:?}"
                );
                // Restore: whatever this pass staged, the rewritten ticket names it.
                let restore = restore_ticket(&removal, (7, 9), ft);
                assert!(ticket_accepts(&restore, ft), "restore {relative} {ft:?}");
                for other in ALL.into_iter().filter(|o| *o != ft) {
                    assert!(
                        !ticket_accepts(&restore, other),
                        "{relative} {ft:?}/{other:?}"
                    );
                }
                // A removal ticket recording another kind is refused even for that kind.
                if ft != expected {
                    let lying = Ticket {
                        file_type: Some(StagedType::of(ft)),
                        ..base.clone()
                    };
                    assert!(!ticket_accepts(&lying, ft), "{relative} {ft:?}");
                }
                // Tickets of earlier builds (no record) keep the expected-kind rule.
                for restore in [false, true] {
                    let old = Ticket {
                        restore,
                        ..base.clone()
                    };
                    assert_eq!(
                        ticket_accepts(&old, ft),
                        ft == expected,
                        "{relative} {ft:?}"
                    );
                }
            }
        }
    }
}
