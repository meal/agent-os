#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{EffectId, EffectRecord};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::agent::{Agent, AgentAction, Observation};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use tempfile::TempDir;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

pub fn fix_patch() -> String {
    fs::read_to_string(fixtures().join("parser-repo.fix.patch")).unwrap()
}

/// A patch that only adds a comment to `src/parser.py`: applies, fixes nothing.
pub fn comment_patch() -> String {
    "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1,2 +1,3 @@\n+# TODO: handle whitespace\n def parse_kv(text: str) -> dict:\n     \"\"\"Parse 'key = value' lines into a dict, skipping blanks and '#' comments.\"\"\"\n".into()
}

/// A syntactically valid patch creating `path` with one line.
pub fn create_patch(path: &str, line: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{line}\n"
    )
}

/// A patch editing the first line of `path` from `old` to `new`.
pub fn edit_patch(path: &str, old: &str, new: &str) -> String {
    format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n")
}

pub const ALL_CAPS: &[&str] = &["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"];

pub fn contract(tool_actions: u32) -> (Contract, Digest) {
    contract_with(tool_actions, ALL_CAPS)
}

pub fn contract_with(tool_actions: u32, caps: &[&str]) -> (Contract, Digest) {
    let caps = serde_json::to_string(caps).unwrap();
    let json = format!(
        r#"{{
        "goal": "fix the parser",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "python-stdlib-v1",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": 1,
            "max_output_tokens_per_request": 1000,
            "tool_actions": {tool_actions},
            "deadline_seconds": 600,
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }}
    }}"#
    );
    (Contract::parse(&json).unwrap(), Digest::of(json.as_bytes()))
}

pub struct Env {
    pub dir: TempDir,
    pub db: Db,
    pub blobs: BlobStore,
    pub exec: FixtureExecutor,
    pub task: TaskId,
    pub contract: Contract,
}

impl Env {
    pub fn new(tool_actions: u32) -> Env {
        Env::with_caps(tool_actions, ALL_CAPS)
    }

    /// An approved task (the owner said yes; handles are issued).
    pub fn with_caps(tool_actions: u32, caps: &[&str]) -> Env {
        let env = Env::unapproved(tool_actions, caps);
        env.db.approve_task(&env.task).unwrap();
        env
    }

    /// A task the owner has not approved yet: no handles, no deadline.
    pub fn unapproved(tool_actions: u32, caps: &[&str]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let exec = FixtureExecutor::new(
            dir.path().join("snapshot"),
            dir.path().join("profile"),
            dir.path().join("work"),
        );
        let (contract, digest) = contract_with(tool_actions, caps);
        let task = db.create_task(&contract, &digest).unwrap();
        Env { dir, db, blobs, exec, task, contract }
    }

    /// A second connection to the same database, as a concurrent writer would hold.
    pub fn second_db(&self) -> Db {
        Db::open(&self.dir.path().join("agentos.db")).unwrap()
    }

    /// Another executor over the same snapshot, profile and work root.
    pub fn fixture_exec(&self) -> FixtureExecutor {
        FixtureExecutor::new(self.snapshot_dir(), self.profile_dir(), self.dir.path().join("work"))
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.dir.path().join("profile")
    }

    pub fn snapshot_dir(&self) -> PathBuf {
        self.dir.path().join("snapshot")
    }

    pub fn ws(&self) -> PathBuf {
        self.exec.workspace(&self.task)
    }

    pub fn ws_digest(&self) -> Digest {
        workspace_digest(&self.ws()).unwrap()
    }

    pub fn events(&self) -> Vec<StoredEvent> {
        self.db.events(&self.task).unwrap()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.event_type).collect()
    }

    pub fn count(&self, event_type: &str) -> usize {
        self.events().iter().filter(|e| e.event_type == event_type).count()
    }

    /// `Denied` audit events whose payload `reason` equals `reason`.
    pub fn denials(&self, reason: &str) -> Vec<serde_json::Value> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "Denied" && e.payload["reason"] == reason)
            .map(|e| e.payload)
            .collect()
    }

    /// Effects of the given kind tag ("ReadSnapshot", "ApplyPatch", ...), in intent order.
    pub fn effects(&self, kind: &str) -> Vec<EffectRecord> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "EffectIntended")
            .filter(|e| e.payload["kind"] == kind || e.payload["kind"].get(kind).is_some())
            .map(|e| {
                let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
                self.db.effect(&id).unwrap()
            })
            .collect()
    }

    pub fn blob_json(&self, d: &Digest) -> serde_json::Value {
        serde_json::from_slice(&self.blobs.get(d).unwrap()).unwrap()
    }
}

/// Agent driven by a closure, for tests that act on the world while the agent "thinks".
pub struct FnAgent<F: FnMut(&Observation) -> AgentAction>(pub F);

impl<F: FnMut(&Observation) -> AgentAction> Agent for FnAgent<F> {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        (self.0)(obs)
    }
}

/// Wraps the fixture executor with hooks that run around each attempt, so a test can act
/// on the world while an effect is in flight (after dispatch, before completion).
pub struct HookExec<B, A> {
    pub inner: FixtureExecutor,
    pub before: B,
    pub after: A,
}

impl<B, A> Executor for HookExec<B, A>
where
    B: Fn(&EffectRequest) + Send + Sync,
    A: Fn(&EffectRequest) + Send + Sync,
{
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        (self.before)(req);
        let out = self.inner.run(req, ctx).await;
        (self.after)(req);
        out
    }
}
