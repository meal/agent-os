//! Runs an `AnalyzeSnapshot` effect in the controller: the contract's analyzer component over
//! the task's recorded snapshot, through `agentos-component`. Every read the component makes
//! is authorized by the broker against the task's `snapshot.analyze` capability and goes
//! through the same path validation as `ReadFile`. The outcome is retained before it is
//! returned, like a model response, so recovery publishes it without running again.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use agentos_component::{
    FileEntry, Limits, Outcome, RUNTIME, Runtime, Snapshot, SnapshotError, Tree,
};
use agentos_core::broker::Resource;
use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::workspace::list_files;
use agentos_store::db::{Db, DbError};
use serde::{Deserialize, Serialize};

use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::retention::Retention;
use crate::shadow::{check_path, open_file};
use crate::supervised::ExecCounts;
use crate::workspace::workspace_digest;

/// The request identity of an analysis: the component and snapshot it runs over, the runtime
/// and every bound. A recorded effect replays only with exactly these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisRequest {
    pub component_digest: Digest,
    pub snapshot_digest: Digest,
    pub runtime: String,
    pub fuel: u64,
    pub memory_bytes: u64,
    pub table_elements: u64,
    pub report_limit: u64,
    pub read_max: u32,
    pub read_calls: u32,
    pub read_bytes: u64,
}

impl AnalysisRequest {
    /// The request for `component` over `snapshot` with this build's runtime and bounds.
    pub fn new(component: Digest, snapshot: Digest) -> AnalysisRequest {
        let l = Limits::default();
        AnalysisRequest {
            component_digest: component,
            snapshot_digest: snapshot,
            runtime: RUNTIME.to_string(),
            fuel: l.fuel,
            memory_bytes: l.memory_bytes as u64,
            table_elements: l.table_elements as u64,
            report_limit: l.report_bytes as u64,
            read_max: l.read_max,
            read_calls: l.read_calls,
            read_bytes: l.read_bytes,
        }
    }

    fn limits(&self) -> Result<Limits, String> {
        let size = |v: u64, what: &str| {
            usize::try_from(v).map_err(|_| format!("analysis request: {what} {v} does not fit"))
        };
        Ok(Limits {
            fuel: self.fuel,
            memory_bytes: size(self.memory_bytes, "memory_bytes")?,
            table_elements: size(self.table_elements, "table_elements")?,
            report_bytes: size(self.report_limit, "report_limit")?,
            read_max: self.read_max,
            read_calls: self.read_calls,
            read_bytes: self.read_bytes,
            ..Limits::default()
        })
    }
}

/// The request payload of `contract`'s analysis over `snapshot`, or `None` without an
/// analyzer. Deterministic, so recovery rebuilds exactly the journaled request (it checks the
/// digest against the intent's).
pub fn request_payload(contract: &Contract, snapshot: Digest) -> Option<Vec<u8>> {
    let analyzer = contract.analyzer.as_ref()?;
    let component = Digest::from_hex(&analyzer.digest).ok()?;
    let request = AnalysisRequest::new(component, snapshot);
    Some(serde_json::to_vec(&request).expect("an analysis request serializes"))
}

/// A task's recorded snapshot as the component sees it.
struct TaskSnapshot {
    db: Db,
    task: TaskId,
    root: PathBuf,
}

impl Snapshot for TaskSnapshot {
    fn authorize(&self) -> Result<(), String> {
        match self
            .db
            .check(&self.task, Capability::SnapshotAnalyze, &Resource::Task)
        {
            Ok(()) => Ok(()),
            Err(DbError::CapabilityDenied { reason, .. }) => Err(reason),
            Err(e) => Err(format!("broker check failed: {e}")),
        }
    }

    fn files(&self) -> Result<Vec<FileEntry>, SnapshotError> {
        let files = list_files(&self.root).map_err(|e| SnapshotError::Io(e.to_string()))?;
        let mut entries = Vec::with_capacity(files.len());
        for (path, full) in files {
            let size = fs::symlink_metadata(&full)
                .map_err(|e| SnapshotError::Io(e.to_string()))?
                .len();
            entries.push(FileEntry { path, size });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn read(&self, path: &str, offset: u64, len: u32) -> Result<Vec<u8>, SnapshotError> {
        check_path(path).map_err(SnapshotError::InvalidPath)?;
        let mut file = open_file(&self.root, path).map_err(|why| {
            if why.contains("crosses symlink") {
                SnapshotError::InvalidPath(why)
            } else if why.starts_with("file not in the workspace") {
                SnapshotError::NotFound
            } else {
                SnapshotError::Io(why)
            }
        })?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| SnapshotError::Io(e.to_string()))?;
        let mut bytes = Vec::with_capacity(len.min(1 << 20) as usize);
        file.take(u64::from(len))
            .read_to_end(&mut bytes)
            .map_err(|e| SnapshotError::Io(e.to_string()))?;
        Ok(bytes)
    }
}

pub struct AnalysisExecutor {
    retention: Retention,
    db_path: PathBuf,
    snapshot_dir: PathBuf,
    component: PathBuf,
    counts: ExecCounts,
    crash: Option<CrashHook>,
}

impl AnalysisExecutor {
    /// Retains outcomes under `root` (`<home>/analysis`); reads the task's recorded snapshot
    /// at `snapshot_dir` and its copy of the analyzer at `component`.
    pub fn new(
        root: PathBuf,
        db_path: PathBuf,
        snapshot_dir: PathBuf,
        component: PathBuf,
        counts: ExecCounts,
    ) -> AnalysisExecutor {
        AnalysisExecutor {
            retention: Retention {
                root,
                file: "outcome.json",
                what: "analysis outcome",
            },
            db_path,
            snapshot_dir,
            component,
            counts,
            crash: None,
        }
    }

    pub fn with_crash(mut self, hook: Option<CrashHook>) -> Self {
        self.crash = hook;
        self
    }

    pub fn retention_dir(&self, effect: &EffectId, attempt: &AttemptId) -> PathBuf {
        self.retention.dir(effect, attempt)
    }
}

/// Runs the checked request. Blocking: compilation and the component run synchronously.
fn analyze(
    request: &AnalysisRequest,
    db_path: &Path,
    snapshot_dir: &Path,
    component: &Path,
    task: &TaskId,
) -> Result<Outcome, String> {
    if request.runtime != RUNTIME {
        return Err(format!(
            "the analysis was requested for {}, this build runs {RUNTIME}",
            request.runtime
        ));
    }
    let bytes = fs::read(component).map_err(|e| format!("cannot read the analyzer: {e}"))?;
    if Digest::of(&bytes) != request.component_digest {
        return Err(format!(
            "the analyzer's bytes are not {}",
            request.component_digest
        ));
    }
    let snapshot =
        workspace_digest(snapshot_dir).map_err(|e| format!("cannot digest the snapshot: {e}"))?;
    if snapshot != request.snapshot_digest {
        return Err(format!(
            "the snapshot is {snapshot}, not {}",
            request.snapshot_digest
        ));
    }
    let limits = request.limits()?;
    let db = Db::open(db_path).map_err(|e| format!("cannot open the journal: {e}"))?;
    let runtime = Runtime::new()?;
    let tree = Tree {
        task: task.to_string(),
        snapshot: Box::new(TaskSnapshot {
            db,
            task: task.clone(),
            root: snapshot_dir.to_path_buf(),
        }),
    };
    Ok(runtime.analyze(&bytes, task.as_str(), tree, limits))
}

impl Executor for AnalysisExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        if !matches!(req.kind, EffectKind::AnalyzeSnapshot) {
            return ExecOutcome::failure(req, ctx, "not an analysis");
        }
        let request: AnalysisRequest = match serde_json::from_slice(&req.payload) {
            Ok(r) => r,
            Err(e) => {
                return ExecOutcome::failure(req, ctx, format!("invalid analysis request: {e}"));
            }
        };
        let (db_path, snapshot_dir, component, task) = (
            self.db_path.clone(),
            self.snapshot_dir.clone(),
            self.component.clone(),
            req.task_id.clone(),
        );
        let ran = tokio::task::spawn_blocking(move || {
            analyze(&request, &db_path, &snapshot_dir, &component, &task)
        })
        .await;
        let out = match ran {
            Ok(Ok(Outcome::Report(report))) => ExecOutcome::success(req, ctx, report.into_bytes()),
            Ok(Ok(Outcome::Failed(why))) => ExecOutcome::failure(req, ctx, why),
            Ok(Ok(Outcome::Infrastructure(why))) => {
                ExecOutcome::failure(req, ctx, format!("infrastructure: {why}"))
            }
            Ok(Err(why)) => ExecOutcome::failure(req, ctx, why),
            Err(e) => ExecOutcome::failure(
                req,
                ctx,
                format!("infrastructure: the analysis panicked: {e}"),
            ),
        };
        self.counts.record(&req.kind);
        if self
            .crash
            .as_ref()
            .is_some_and(|h| h.check(CrashPoint::DuringExecute, Some("analyze_snapshot")))
        {
            return ExecOutcome::failure(
                req,
                ctx,
                "injected crash before the outcome was retained",
            );
        }
        self.retention.retain(req, ctx, &out);
        out
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.retention.retained(effect)
    }
}
