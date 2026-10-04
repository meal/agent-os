//! Conservative collection of reconstructible transient paths. Caller holds driver.lock.
//!
//! No journal, input, registry or blob is removed. A refusal aborts the entire deletion
//! pass. Inspection/jail cleanup remains the worker's responsibility.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_store::{blob::BlobStore, db::Db};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::executor::ExecOutcome;
use crate::job::{JobDir, JobRequest, WorkerConfig};

const REPORT_LIMIT: usize = 1000;
const TREE_LIMIT: usize = 10000;
const JSON_LIMIT: u64 = 32 * 1024 * 1024;
mod confined;

#[derive(Debug, Serialize)]
pub struct Entry {
    pub path: String,
    pub status: String,
    pub reason: String,
}
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub entries: Vec<Entry>,
}

/// Scoped collection diagnostic boundaries, without global/environment switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStage {
    Validated,
    Staged,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum Kind {
    Job,
    Model,
    Workspace,
}
struct Candidate {
    path: PathBuf,
    relative: PathBuf,
    parent: File,
    kind: Kind,
    pending: Option<Pending>,
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
#[derive(Serialize, Deserialize)]
struct Ticket {
    version: u32,
    relative: PathBuf,
    kind: Kind,
    proof: Proof,
    device: u64,
    inode: u64,
}
struct Pending {
    ticket: Ticket,
    needs_move: bool,
}
struct Collector<'a> {
    anchor: &'a File,
    db: &'a Db,
    blobs: &'a BlobStore,
    referenced: HashSet<Digest>,
    locks: Vec<(PathBuf, File)>,
    work_dirs: HashMap<TaskId, File>,
}

fn refuse(reason: impl Into<String>) -> io::Error {
    io::Error::other(reason.into())
}

fn bounded_reason(error: impl std::fmt::Display) -> String {
    let mut reason = error.to_string();
    if reason.len() > 1024 {
        let mut end = 1024;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push('…');
    }
    reason
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
fn plain_dir(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(refuse("expected an owned directory; symlinks refused"));
    }
    Ok(())
}
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > JSON_LIMIT {
        return Err(refuse("unsafe or oversized receipt/request file"));
    }
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?
        .take(JSON_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > JSON_LIMIT {
        return Err(refuse("receipt/request too large"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Refuse redirecting paths and data with another filesystem owner. Only directories
/// and single-link regular files are supported in this first collector.
fn check_tree(path: &Path, firecracker_socket: bool) -> io::Result<()> {
    let job_root = path;
    let mut stack = vec![path.to_path_buf()];
    let mut count = 0;
    while let Some(path) = stack.pop() {
        count += 1;
        if count > TREE_LIMIT {
            return Err(refuse("candidate tree exceeds 10000 entries"));
        }
        confined::refuse_mount(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                if count + stack.len() >= TREE_LIMIT {
                    return Err(refuse("candidate tree exceeds 10000 entries"));
                }
                stack.push(entry?.path());
            }
        } else if firecracker_socket
            && meta.file_type().is_socket()
            && meta.nlink() == 1
            && path.parent() == Some(job_root)
            && path.file_name().is_some_and(|n| n == "v.sock")
        {
            // A Firecracker worker leaves its owned socket after shutting down the VM.
            // Job/workspace locks and published receipt are checked before removal.
        } else if !meta.is_file() || meta.nlink() != 1 {
            return Err(refuse(format!(
                "symlink, special file or hard link refused: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

impl Collector<'_> {
    fn lock(&mut self, path: &Path, create: bool) -> io::Result<()> {
        if let Some((_, file)) = self.locks.iter().find(|(p, _)| p == path) {
            let held = file.metadata()?;
            let current = fs::symlink_metadata(path)?;
            if current.is_file()
                && current.nlink() == 1
                && held.ino() == current.ino()
                && held.dev() == current.dev()
            {
                return Ok(());
            }
            return Err(refuse("lock inode changed"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(path)?;
        if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
            return Err(refuse("nonregular or hard-linked lock refused"));
        }
        file.try_lock()
            .map_err(|e| refuse(format!("lock unavailable: {e}")))?;
        self.locks.push((path.to_path_buf(), file));
        self.lock(path, false)
    }

    fn task(&mut self, task: &TaskId) -> io::Result<()> {
        if !uuid_name(task.as_str()) {
            return Err(refuse("unknown task directory name"));
        }
        let t = self.db.task(task).map_err(io::Error::other)?;
        if !t.state.is_terminal() || (t.cancel_requested && t.state != TaskState::Cancelled) {
            return Err(refuse("task is active or cancellation is pending"));
        }
        if !self
            .db
            .outstanding_effects(task)
            .map_err(io::Error::other)?
            .is_empty()
        {
            return Err(refuse("task has outstanding effects"));
        }
        if exists(&confined::path(self.anchor).join("inspect"))? {
            let inspect_root = confined::open_dir(self.anchor, "inspect".as_ref())?;
            if exists(&confined::path(&inspect_root).join(task.as_str()))? {
                let inspect = confined::open_dir(&inspect_root, task.as_str().as_ref())?;
                if fs::read_dir(confined::path(&inspect))?.next().is_some() {
                    return Err(refuse("inspection leftovers retained"));
                }
            }
        }
        if exists(&confined::path(self.anchor).join("work"))? {
            let work_root = confined::open_dir(self.anchor, "work".as_ref())?;
            if exists(&confined::path(&work_root).join(task.as_str()))? {
                let current = confined::open_dir(&work_root, task.as_str().as_ref())?;
                if let Some(held) = self.work_dirs.get(task) {
                    let a = held.metadata()?;
                    let b = current.metadata()?;
                    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
                        return Err(refuse("workspace parent changed"));
                    }
                } else {
                    self.work_dirs.insert(task.clone(), current);
                }
                let lock_path = confined::path(self.work_dirs.get(task).expect("inserted above"))
                    .join("ws.lock");
                self.lock(&lock_path, true)?;
            }
        }
        Ok(())
    }

    fn outcome(
        &mut self,
        path: &Path,
        out: &ExecOutcome,
        expected_kind: Option<&EffectKind>,
    ) -> io::Result<EffectRecord> {
        let rec = self
            .db
            .effect(&out.receipt.effect_id)
            .map_err(io::Error::other)?;
        if !matches!(rec.state, EffectState::Completed | EffectState::Failed)
            || out.unresolved
            || rec.result_digest != out.receipt.result_digest
            || rec.result_digest != Some(Digest::of(&out.output))
            || rec.lease_generation != out.receipt.lease_generation
            || expected_kind.is_some_and(|kind| *kind != rec.kind)
        {
            return Err(refuse("receipt does not match a settled effect"));
        }
        let effect = rec.effect_id.as_str();
        let attempt = out.receipt.attempt_id.to_string();
        if effect.len() != 64
            || !effect
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !uuid_name(&attempt)
            || path.file_name().and_then(|n| n.to_str()) != Some(&format!("{effect}-{attempt}"))
        {
            return Err(refuse(
                "directory does not match effect and attempt ownership",
            ));
        }
        let digest = rec.result_digest.expect("checked above");
        if !self.referenced.contains(&digest) || self.blobs.get(&digest)? != out.output {
            return Err(refuse("published receipt blob unavailable"));
        }
        self.task(&rec.task_id)?;
        Ok(rec)
    }

    fn validate_proof(&mut self, ticket: &Ticket) -> io::Result<()> {
        if ticket.version != 1 {
            return Err(refuse("unknown deletion ticket version"));
        }
        let parts: Vec<_> = ticket.relative.components().collect();
        if parts
            .iter()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err(refuse("invalid deletion ticket path"));
        }
        let text = ticket.relative.to_string_lossy();
        match ticket.kind {
            Kind::Workspace => {
                if parts.len() != 3
                    || parts[0].as_os_str() != "work"
                    || parts[1].as_os_str() != ticket.proof.task.as_str()
                    || !["ws", "workspace", "ws.img"]
                        .iter()
                        .any(|name| parts[2].as_os_str() == *name)
                    || ticket.proof.effect.is_some()
                    || ticket.proof.result.is_some()
                {
                    return Err(refuse("invalid workspace deletion ticket"));
                }
            }
            Kind::Job | Kind::Model => {
                let (Some(effect), Some(attempt), Some(result)) = (
                    &ticket.proof.effect,
                    &ticket.proof.attempt,
                    &ticket.proof.result,
                ) else {
                    return Err(refuse("incomplete deletion ticket"));
                };
                let expected = format!(
                    "{}/{}-{}",
                    if matches!(ticket.kind, Kind::Job) {
                        "jobs"
                    } else {
                        "model"
                    },
                    effect,
                    attempt
                );
                if text != expected {
                    return Err(refuse("deletion ticket ownership mismatch"));
                }
                let rec = self.db.effect(effect).map_err(io::Error::other)?;
                if rec.task_id != ticket.proof.task
                    || !matches!(rec.state, EffectState::Completed | EffectState::Failed)
                    || rec.result_digest != Some(*result)
                    || rec.lease_generation != ticket.proof.lease
                    || !self.referenced.contains(result)
                    || (matches!(ticket.kind, Kind::Model)
                        && !matches!(rec.kind, EffectKind::ModelCall { .. }))
                {
                    return Err(refuse(
                        "deletion ticket effect is not settled and published",
                    ));
                }
                self.blobs.get(result)?;
            }
        }
        self.task(&ticket.proof.task)
    }

    fn validate(&mut self, candidate: &Candidate) -> io::Result<Proof> {
        if let Some(pending) = &candidate.pending {
            self.validate_proof(&pending.ticket)?;
            if !pending.needs_move {
                if exists(&candidate.path)? {
                    let meta = fs::symlink_metadata(&candidate.path)?;
                    if (meta.dev(), meta.ino()) != (pending.ticket.device, pending.ticket.inode) {
                        return Err(refuse("staged inode differs from deletion ticket"));
                    }
                    check_tree(&candidate.path, pending.ticket.proof.firecracker_socket)?;
                }
                return Ok(pending.ticket.proof.clone());
            }
        }
        let socket = if matches!(candidate.kind, Kind::Job) {
            plain_dir(&candidate.path)?;
            let req: JobRequest = read_json(&candidate.path.join("request.json"))?;
            matches!(req.worker, WorkerConfig::Firecracker(_))
        } else {
            false
        };
        check_tree(&candidate.path, socket)?;
        let proof = match candidate.kind {
            Kind::Job => {
                if exists(&candidate.path.join("jail"))? {
                    return Err(refuse("jail leftovers retained"));
                }
                self.lock(&candidate.path.join("lock"), false)?;
                let req: JobRequest = read_json(&candidate.path.join("request.json"))?;
                let job = JobDir::open(&candidate.path)?;
                for name in ["receipt.json", "output.bin"] {
                    if fs::symlink_metadata(candidate.path.join(name))?.len() > JSON_LIMIT {
                        return Err(refuse("job receipt too large"));
                    }
                }
                let out = job
                    .read_receipt()
                    .ok_or_else(|| refuse("missing or invalid job receipt"))?;
                let rec = self.outcome(&candidate.path, &out, Some(&req.kind))?;
                if rec.effect_id != req.effect_id
                    || rec.task_id != req.task_id
                    || out.receipt.attempt_id != req.attempt_id
                    || out.receipt.lease_generation != req.lease_generation
                {
                    return Err(refuse("job request ownership mismatch"));
                }
                Proof {
                    task: rec.task_id,
                    effect: Some(rec.effect_id),
                    attempt: Some(req.attempt_id),
                    result: rec.result_digest,
                    lease: rec.lease_generation,
                    firecracker_socket: socket,
                }
            }
            Kind::Model => {
                let out: ExecOutcome = read_json(&candidate.path.join("response.json"))?;
                let rec = self.outcome(&candidate.path, &out, None)?;
                if !matches!(rec.kind, EffectKind::ModelCall { .. }) {
                    return Err(refuse("retention is not a model effect"));
                }
                Proof {
                    task: rec.task_id,
                    effect: Some(rec.effect_id),
                    attempt: Some(out.receipt.attempt_id),
                    result: rec.result_digest,
                    lease: rec.lease_generation,
                    firecracker_socket: false,
                }
            }
            Kind::Workspace => {
                let name = candidate
                    .relative
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| refuse("invalid workspace owner"))?;
                if !uuid_name(name) {
                    return Err(refuse("invalid workspace owner"));
                }
                let task: TaskId = serde_json::from_value(serde_json::Value::String(name.into()))
                    .map_err(io::Error::other)?;
                self.task(&task)?;
                Proof {
                    task,
                    effect: None,
                    attempt: None,
                    result: None,
                    lease: 0,
                    firecracker_socket: false,
                }
            }
        };
        if let Some(pending) = &candidate.pending {
            let meta = fs::symlink_metadata(&candidate.path)?;
            if proof != pending.ticket.proof
                || (meta.dev(), meta.ino()) != (pending.ticket.device, pending.ticket.inode)
            {
                return Err(refuse("source differs from durable deletion ticket"));
            }
        }
        Ok(proof)
    }
}

fn hex_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn pending(root: &File) -> io::Result<Vec<Candidate>> {
    let trash_path = confined::path(root).join("gc-trash");
    if !exists(&trash_path)? {
        return Ok(Vec::new());
    }
    let trash = confined::open_dir(root, "gc-trash".as_ref())?;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(confined::path(&trash))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmp") && entry.file_type()?.is_file() {
            continue;
        }
        let Some(key) = name.strip_suffix(".json") else {
            if hex_name(&name)
                && entry.file_type()?.is_dir()
                && exists(&confined::path(&trash).join(format!("{name}.json")))?
            {
                continue;
            }
            return Err(refuse("unowned entry in deletion staging"));
        };
        if !hex_name(key) {
            return Err(refuse("unknown deletion ticket name"));
        }
        let ticket: Ticket = read_json(&entry.path())?;
        if Digest::of(ticket.relative.as_os_str().as_encoded_bytes()).to_string() != key {
            return Err(refuse("deletion ticket path pin mismatch"));
        }
        // Validate traversal before passing any ticket path to openat.
        if ticket
            .relative
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err(refuse("deletion ticket traversal refused"));
        }
        let (parent, path, needs_move) = if exists(&confined::path(&trash).join(key))? {
            let dir = confined::open_dir(&trash, key.as_ref())?;
            if exists(&confined::path(&dir).join("data"))? {
                let path = confined::path(&dir).join("data");
                (dir, path, false)
            } else {
                let origin = confined::parent(root, &ticket.relative)?;
                let path = confined::path(&origin).join(
                    ticket
                        .relative
                        .file_name()
                        .ok_or_else(|| refuse("missing source name"))?,
                );
                let present = exists(&path)?;
                (origin, path, present)
            }
        } else {
            let origin = confined::parent(root, &ticket.relative)?;
            let path = confined::path(&origin).join(
                ticket
                    .relative
                    .file_name()
                    .ok_or_else(|| refuse("missing source name"))?,
            );
            let present = exists(&path)?;
            (origin, path, present)
        };
        candidates.push(Candidate {
            path,
            relative: ticket.relative.clone(),
            parent,
            kind: ticket.kind,
            pending: Some(Pending { ticket, needs_move }),
        });
        if candidates.len() > REPORT_LIMIT {
            return Err(refuse("report exceeds 1000 candidates"));
        }
    }
    Ok(candidates)
}

fn scan(root: &File) -> io::Result<Vec<Candidate>> {
    let mut candidates = pending(root)?;
    let retained: HashSet<_> = candidates.iter().map(|c| c.relative.clone()).collect();
    for (sub, kind) in [
        ("jobs", Kind::Job),
        ("model", Kind::Model),
        ("work", Kind::Workspace),
        ("inspect", Kind::Workspace),
    ] {
        if !exists(&confined::path(root).join(sub))? {
            continue;
        }
        let dir = confined::open_dir(root, sub.as_ref())?;
        if sub == "inspect" {
            continue;
        }
        for entry in fs::read_dir(confined::path(&dir))? {
            let entry = entry?;
            if sub == "work" {
                let task_dir = confined::open_dir(&dir, &entry.file_name())?;
                for name in ["ws", "workspace", "ws.img"] {
                    let path = confined::path(&task_dir).join(name);
                    let relative = Path::new(sub).join(entry.file_name()).join(name);
                    if exists(&path)? && !retained.contains(&relative) {
                        let parent = task_dir.try_clone()?;
                        let path = confined::path(&parent).join(name);
                        candidates.push(Candidate {
                            path,
                            relative,
                            parent,
                            kind,
                            pending: None,
                        });
                    }
                }
            } else {
                let relative = Path::new(sub).join(entry.file_name());
                if !retained.contains(&relative) {
                    let parent = dir.try_clone()?;
                    let path = confined::path(&parent).join(entry.file_name());
                    candidates.push(Candidate {
                        path,
                        relative,
                        parent,
                        kind,
                        pending: None,
                    });
                }
            }
            if candidates.len() > REPORT_LIMIT {
                return Err(refuse("report exceeds 1000 candidates; no data deleted"));
            }
        }
    }
    candidates.sort_by_key(|c| (matches!(c.kind, Kind::Workspace), c.relative.clone()));
    Ok(candidates)
}

fn publish_ticket(trash: &File, key: &str, ticket: &Ticket) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(confined::path(trash))?;
    tmp.write_all(&serde_json::to_vec(ticket).map_err(io::Error::other)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(confined::path(trash).join(format!("{key}.json")))
        .map_err(|e| e.error)?;
    trash.sync_all()
}

fn delete(
    root: &File,
    candidate: &Candidate,
    proof: Proof,
    identity: (u64, u64),
    hook: &dyn Fn(CollectionStage, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let trash = confined::directory(root, "gc-trash".as_ref())?;
    let key = Digest::of(candidate.relative.as_os_str().as_encoded_bytes()).to_string();
    let ticket;
    let needs_move;
    if let Some(pending) = &candidate.pending {
        ticket = &pending.ticket;
        needs_move = pending.needs_move;
    } else {
        let new_ticket = Ticket {
            version: 1,
            relative: candidate.relative.clone(),
            kind: candidate.kind,
            proof,
            device: identity.0,
            inode: identity.1,
        };
        publish_ticket(&trash, &key, &new_ticket)?;
        return delete_new(&trash, &key, &new_ticket, candidate, true, hook);
    }
    delete_new(&trash, &key, ticket, candidate, needs_move, hook)
}

fn delete_new(
    trash: &File,
    key: &str,
    ticket: &Ticket,
    candidate: &Candidate,
    needs_move: bool,
    hook: &dyn Fn(CollectionStage, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let stage = confined::directory(trash, key.as_ref())?;
    if needs_move {
        confined::move_entry(
            &candidate.parent,
            candidate.relative.file_name().expect("validated name"),
            &stage,
        )?;
    }
    let data = confined::path(&stage).join("data");
    if exists(&data)? {
        let meta = fs::symlink_metadata(&data)?;
        if (meta.dev(), meta.ino()) != (ticket.device, ticket.inode) {
            return Err(refuse("staged inode changed; data retained"));
        }
        check_tree(&data, ticket.proof.firecracker_socket)?;
        hook(CollectionStage::Staged, &candidate.relative)?;
        let mut remaining = TREE_LIMIT;
        confined::remove(&stage, "data".as_ref(), &mut remaining)?;
    }
    // The ticket outlives payload and staging directory removal. Any interruption
    // leaves enough ownership proof to retry without the original receipt files.
    rustix::fs::unlinkat(trash, key, rustix::fs::AtFlags::REMOVEDIR)?;
    trash.sync_all()?;
    rustix::fs::unlinkat(trash, format!("{key}.json"), rustix::fs::AtFlags::empty())?;
    trash.sync_all()
}

/// Caller MUST hold the home driver lock throughout. Dry-run creates missing workspace
/// lock files but removes nothing. Any pre-deletion refusal retains the whole pass.
pub fn collect(root: &Path, db: &Db, blobs: &BlobStore, dry_run: bool) -> io::Result<Report> {
    collect_with_hook(root, db, blobs, dry_run, &|_, _| Ok(()))
}

/// Collection with a scoped diagnostic hook for interruption/path-substitution probes.
pub fn collect_with_hook(
    root: &Path,
    db: &Db,
    blobs: &BlobStore,
    dry_run: bool,
    hook: &dyn Fn(CollectionStage, &Path) -> io::Result<()>,
) -> io::Result<Report> {
    let mut report = Report {
        dry_run,
        entries: Vec::new(),
    };
    let anchor = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW).bits() as i32)
        .open(root)
    {
        Ok(anchor) => anchor,
        Err(e) => {
            report.entries.push(Entry {
                path: ".".into(),
                status: "refused".into(),
                reason: bounded_reason(&e),
            });
            return Ok(report);
        }
    };
    let candidates = match scan(&anchor) {
        Ok(candidates) => candidates,
        Err(e) => {
            report.entries.push(Entry {
                path: ".".into(),
                status: "refused".into(),
                reason: bounded_reason(&e),
            });
            return Ok(report);
        }
    };
    let mut collector = Collector {
        anchor: &anchor,
        db,
        blobs,
        referenced: db.referenced_blobs().map_err(io::Error::other)?,
        locks: Vec::new(),
        work_dirs: HashMap::new(),
    };
    for digest in &collector.referenced {
        if let Err(e) = blobs.get(digest) {
            report.entries.push(Entry {
                path: "blobs".into(),
                status: "refused".into(),
                reason: bounded_reason(&e),
            });
            return Ok(report);
        }
    }
    let mut blocked = false;
    for candidate in &candidates {
        let result = collector.validate(candidate);
        blocked |= result.is_err();
        report.entries.push(Entry {
            path: candidate.relative.to_string_lossy().into_owned(),
            status: if result.is_ok() {
                "candidate"
            } else {
                "refused"
            }
            .into(),
            reason: result.err().map_or_else(
                || "settled transient copy; durable artifacts retained".into(),
                |e| bounded_reason(&e),
            ),
        });
    }
    if blocked || dry_run {
        return Ok(report);
    }
    let mut proofs = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        match collector.validate(candidate) {
            Ok(proof) => {
                let identity = if let Some(pending) = &candidate.pending {
                    (pending.ticket.device, pending.ticket.inode)
                } else {
                    let meta = fs::symlink_metadata(&candidate.path)?;
                    (meta.dev(), meta.ino())
                };
                proofs.push((proof, identity));
            }
            Err(e) => {
                report.entries[i].status = "refused".into();
                report.entries[i].reason = bounded_reason(&e);
                return Ok(report);
            }
        }
    }
    if let Err(e) = hook(CollectionStage::Validated, Path::new(".")) {
        if let Some(entry) = report.entries.first_mut() {
            entry.status = "refused".into();
            entry.reason = bounded_reason(&e);
        } else {
            report.entries.push(Entry {
                path: ".".into(),
                status: "refused".into(),
                reason: bounded_reason(&e),
            });
        }
        return Ok(report);
    }
    for (i, (candidate, (proof, identity))) in candidates.iter().zip(proofs).enumerate() {
        match delete(&anchor, candidate, proof, identity, hook) {
            Ok(()) => report.entries[i].status = "deleted".into(),
            Err(e) => {
                report.entries[i].status = "refused".into();
                report.entries[i].reason = bounded_reason(&e);
                break;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod mount_tests {
    use super::*;
    #[test]
    fn mounted_candidate_data_is_refused() {
        let Some(root) = std::env::var_os("AGENTOS_GC_MOUNT_FIXTURE") else {
            return;
        };
        let root = Path::new(&root);
        assert!(
            check_tree(root, false).is_err(),
            "mounted data must not be collectable"
        );
        assert_eq!(
            fs::read(root.join("mounted/foreign")).unwrap(),
            b"foreign data must remain"
        );
    }
}
