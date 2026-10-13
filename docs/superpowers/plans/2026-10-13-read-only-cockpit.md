# Read-only cockpit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `agentos cockpit` opens a read-only, auto-refreshing terminal UI that lists every workflow and task in a home directory with state and cost, and drills into a workflow's steps or a task's events.

**Architecture:** A new read-only store type (`agentos_store::View`) opens the SQLite file with `SQLITE_OPEN_READ_ONLY` and `query_only`, takes no lock, never migrates, and answers list queries. A new crate `agentos-cockpit` (ratatui) owns a pure app/render layer and a runtime (a worker thread polling the `View`). `agentos-cli` gets a thin `cockpit` command. A workflow's state comes from the one existing derivation in `agentos-core`.

**Tech Stack:** Rust 2024 workspace, rusqlite 0.40.2, ratatui 0.30.2 (latest on crates.io, checked 2026-10-13; its default features include the crossterm backend, re-exported as `ratatui::crossterm`).

**Spec:** `docs/superpowers/specs/2026-10-13-read-only-cockpit-design.md`

**Rulings against the spec (binding for this plan):**
- The spec writes `Db::open_read_only`. This plan makes it a separate type, `View`, because `Db` exposes write methods; a type that has none makes "read-only" a compile-time fact, not only a SQLite flag. Task 6 updates the spec wording.
- Event paging: the spec says events "load in pages of 200 as the view scrolls". This plan shows the latest 200 events and loads an older page of 200 when the selection reaches the top (`EventPage::Before`). New events are appended on refresh (`EventPage::After`).

## Global Constraints

- All builds and tests run through `docker compose run --rm test ...` (host cargo cannot build: `target/` is root-owned).
- Run `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` before each commit; both must be clean.
- No co-author lines in commits (`Do not co-author commits`).
- The cockpit never writes: no `Db`, no `Store`, no `Home::open` (it creates directories), no home lock, no network.
- Never create a missing home or database file.
- Every string that comes from the store is passed through `sanitize::clean` before it is drawn.
- Event payloads are shown only on `p`, one event at a time, capped at 64 KiB of text.
- Library versions: ratatui `0.30.2`. If the compiler reports a renamed item in ratatui 0.30.2, check docs.rs/ratatui/0.30.2 and keep the behavior; do not downgrade.
- Fixture/test style: follow the neighbouring tests; keep test output free of warnings.

## Review Focus

Failure modes the spec implies that no task's happy path covers. Each has a test in the task named.

1. A goal, step name or event type holding ESC sequences, newlines, bidi overrides or C1 controls must not reach the screen as controls (Task 3 sanitizer, Task 4 render).
2. A home with no database file, or a database of another schema version, must be refused without creating or migrating anything (Task 2).
3. A controller writing while the cockpit reads: reads see new rows, and no write ever succeeds through the view (Task 2).
4. A workflow failed by `fail_workflow` (options `failure`) must show FAILED, a workflow not yet approved READY, and its step tasks must not also appear as top-level rows (Tasks 3, 4).
5. A tiny terminal (for example 20x5) and multibyte text (CJK, emoji) must not panic on truncation or layout (Task 3 truncation, Task 4 render).
6. A huge payload and a huge number of tasks must be capped (Tasks 2, 4).

---

### Task 1: One workflow-state derivation in core

The CLI's `state_of` (crates/agentos-cli/src/commands/workflow.rs) turns step statuses into a `WorkflowState`, including the "failed by `fail_workflow`" case, and its `label` adds READY. The cockpit needs both, so they move to core and the CLI calls them.

**Files:**
- Modify: `crates/agentos-core/src/workflow.rs` (add `state_of_steps`, `display_label`; tests in its `mod tests`)
- Modify: `crates/agentos-cli/src/commands/workflow.rs` (`state_of` and `label` use them)

**Interfaces:**
- Produces: `pub fn state_of_steps(steps: &[StepStatus], failed: bool) -> WorkflowState` and `pub fn display_label(state: WorkflowState, approved: bool) -> &'static str` in `agentos_core::workflow`.
- Consumes: existing `derive_state`, `StepStatus`, `WorkflowState::label()`.

- [ ] **Step 1: Write the failing tests** (append to the existing `#[cfg(test)] mod tests` in `crates/agentos-core/src/workflow.rs`)

```rust
    #[test]
    fn a_recorded_failure_fails_the_chain_at_its_first_skipped_step() {
        let steps = [
            StepStatus::Task(TaskState::Succeeded),
            StepStatus::Skipped,
            StepStatus::Skipped,
        ];
        assert_eq!(state_of_steps(&steps, true), WorkflowState::Failed { step: 1 });
    }

    #[test]
    fn a_recorded_failure_without_a_skipped_step_fails_the_last_one() {
        let steps = [StepStatus::Task(TaskState::Succeeded), StepStatus::Pending];
        assert_eq!(state_of_steps(&steps, true), WorkflowState::Failed { step: 1 });
    }

    #[test]
    fn without_a_recorded_failure_the_state_is_derived() {
        let steps = [StepStatus::Task(TaskState::Succeeded), StepStatus::Pending];
        assert_eq!(state_of_steps(&steps, false), WorkflowState::Running);
    }

    #[test]
    fn a_running_chain_nobody_approved_is_ready() {
        assert_eq!(display_label(WorkflowState::Running, false), "READY");
        assert_eq!(
            display_label(WorkflowState::Running, true),
            WorkflowState::Running.label()
        );
        assert_eq!(
            display_label(WorkflowState::Succeeded, false),
            WorkflowState::Succeeded.label()
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-core workflow::tests`
Expected: FAIL to compile, `state_of_steps` / `display_label` not found.

- [ ] **Step 3: Implement** (in `crates/agentos-core/src/workflow.rs`, right after `derive_state`)

```rust
/// The chain's state. `failed` is a failure the store recorded without any step showing it
/// (`fail_workflow`: a handoff could not be built); it fails the chain at its first skipped
/// step, or at the last step if none is skipped.
pub fn state_of_steps(steps: &[StepStatus], failed: bool) -> WorkflowState {
    if failed {
        let step = steps
            .iter()
            .position(|s| *s == StepStatus::Skipped)
            .unwrap_or(steps.len().saturating_sub(1));
        return WorkflowState::Failed { step };
    }
    derive_state(steps)
}

/// `READY` for a chain that is not approved yet, else the state's own label.
pub fn display_label(state: WorkflowState, approved: bool) -> &'static str {
    if state == WorkflowState::Running && !approved {
        "READY"
    } else {
        state.label()
    }
}
```

Then replace the bodies in `crates/agentos-cli/src/commands/workflow.rs`: in `state_of`, replace the `let state = match w.options.get("failure") { ... };` block with `let state = state_of_steps(&steps, w.options.get("failure").is_some());` and make `label` call `display_label(state, w.approved)`. Import the two functions from `agentos_core::workflow`. Drop imports that become unused.

- [ ] **Step 4: Run to verify they pass, and nothing else moved**

Run: `docker compose run --rm test cargo test -p agentos-core -p agentos-cli workflow`
Expected: PASS (the existing workflow CLI tests prove the CLI behavior is unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-core/src/workflow.rs crates/agentos-cli/src/commands/workflow.rs
git commit -m "refactor: workflow state and its display label live in core"
```

---

### Task 2: `agentos_store::View`, the read-only view

**Files:**
- Create: `crates/agentos-store/src/view.rs`
- Modify: `crates/agentos-store/src/lib.rs` (add `pub mod view;` next to the other modules)
- Modify: `crates/agentos-store/src/db.rs` (make `state_from_str` and `digest_from_str` `pub(crate)` if they are not already)

**Interfaces:**
- Consumes: `SCHEMA_VERSION`, `state_from_str`, `Contract`, `ChainSpec`, `TaskId`, `WorkflowId`, `TaskState`, `state_of_steps`, `StepStatus`.
- Produces (all `pub` in `agentos_store::view`):

```rust
pub struct View { /* private rusqlite::Connection */ }
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("no agent-os database at {0}")] Missing(PathBuf),
    #[error("database schema version {found} is not {expected}; run any other agentos command to upgrade it")]
    Schema { found: i64, expected: i64 },
    #[error("sqlite: {0}")] Sqlite(#[from] rusqlite::Error),
    #[error("corrupt stored value: {0}")] Corrupt(String),
}
pub struct Snapshot { pub tasks: Vec<TaskRow>, pub workflows: Vec<WorkflowRow>, pub more: bool }
pub struct TaskRow {
    pub id: TaskId, pub state: TaskState, pub cancel_requested: bool, pub goal: String,
    pub model_limit: u32, pub tool_limit: u32, pub model_used: u64, pub tool_used: u64,
    pub created_ts: i64, pub workflow: Option<(WorkflowId, usize)>,
}
pub struct StepView {
    pub idx: usize, pub name: String, pub task_id: Option<TaskId>, pub skipped: bool,
    pub task_state: Option<TaskState>,
    pub model_limit: u32, pub tool_limit: u32, pub model_used: u64, pub tool_used: u64,
}
pub struct WorkflowRow {
    pub id: WorkflowId, pub name: String, pub approved: bool, pub cancel_requested: bool,
    pub failed: bool, pub created_ts: i64, pub steps: Vec<StepView>,
}
impl WorkflowRow { pub fn state(&self) -> WorkflowState; }
pub struct EventRow { pub seq: u64, pub event_type: String, pub ts: i64 }
pub enum EventPage { Latest, Before(u64), After(u64) }
impl View {
    pub fn open(path: &Path) -> Result<View, ViewError>;
    pub fn snapshot(&self, limit: usize) -> Result<Snapshot, ViewError>;
    pub fn contract(&self, task: &TaskId) -> Result<Option<Contract>, ViewError>;
    pub fn events(&self, task: &TaskId, page: EventPage, limit: usize) -> Result<Vec<EventRow>, ViewError>;   // ascending seq
    pub fn event_count(&self, task: &TaskId) -> Result<u64, ViewError>;
    pub fn payload(&self, task: &TaskId, seq: u64, max_chars: usize) -> Result<Option<(String, bool)>, ViewError>; // (text, truncated)
}
```

Semantics: `snapshot` runs inside ONE deferred read transaction so the lists agree with each other. `tasks` holds the newest `limit` tasks by `created_ts DESC, rowid DESC`; `workflows` the newest `limit` workflows the same way; `more` is true if either table held more rows than `limit`. A task's `model_used`/`tool_used` sums its usage rows the way `usage_totals` in `effects.rs` does: `Settled` rows count their settled amount (NULL as 0), `Released` rows count nothing, every other row counts its reserved amount. A `StepView`'s limits come from `spec.steps[idx].contract.limits` (`model_requests`, `tool_actions`), its used counts from its task's usage, and `task_state` from a join (so a step whose task is outside the newest `limit` tasks still has its state). `WorkflowRow.failed` is true when `options_json` has a `"failure"` key. `WorkflowRow::state()` builds `StepStatus` (`Task(state)` / `Skipped` when the step has no task and `skipped` / `Pending`) and calls `agentos_core::workflow::state_of_steps(&steps, self.failed)`.

- [ ] **Step 1: Write the failing tests** (create `crates/agentos-store/src/view.rs` with the types above stubbed with `todo!()` bodies where needed to compile, and this `mod tests` at the bottom). For the contract / workflow fixtures copy the builders already used in `crates/agentos-store/tests/db.rs` (a `Contract` value) and in `crates/agentos-store/src/workflows.rs` `mod tests` (a `ChainSpec`, `create_workflow`, `approve_workflow`, `create_step_task`).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn home() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentos.db");
        (dir, path)
    }

    #[test]
    fn a_missing_database_is_refused_and_not_created() {
        let (_dir, path) = home();
        assert!(matches!(View::open(&path), Err(ViewError::Missing(p)) if p == path));
        assert!(!path.exists());
        assert!(!path.with_extension("db-wal").exists());
    }

    #[test]
    fn another_schema_version_is_refused_and_left_alone() {
        let (_dir, path) = home();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch("CREATE TABLE tasks(id TEXT);").unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            View::open(&path),
            Err(ViewError::Schema { found: 2, expected: 3 })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before, "no migration");
    }

    #[test]
    fn no_write_succeeds_through_the_view_connection() {
        let (_dir, path) = home();
        Db::open(&path).unwrap();
        let view = View::open(&path).unwrap();
        for sql in [
            "INSERT INTO workflows(id, owner, spec_digest, spec_json, options_json, created_ts) VALUES('x','o','d','{}','{}',0)",
            "DELETE FROM tasks",
            "CREATE TABLE evil(a)",
            "PRAGMA user_version = 9",
        ] {
            assert!(view.conn.execute_batch(sql).is_err(), "{sql} must fail");
        }
    }

    #[test]
    fn a_write_made_after_the_open_is_visible() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let view = View::open(&path).unwrap();
        assert!(view.snapshot(10).unwrap().tasks.is_empty());
        db.create_task(&crate::testing::contract(), &crate::testing::digest()).unwrap();
        assert_eq!(view.snapshot(10).unwrap().tasks.len(), 1);
    }

    #[test]
    fn the_newest_rows_come_first_and_more_says_when_there_are_others() {
        let (_dir, path) = home();
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(100));
        let t = ticks.clone();
        let db = Db::open(&path)
            .unwrap()
            .with_clock(Box::new(move || t.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let ids: Vec<_> = (0..3)
            .map(|_| db.create_task(&crate::testing::contract(), &crate::testing::digest()).unwrap())
            .collect();
        let snap = View::open(&path).unwrap().snapshot(2).unwrap();
        assert_eq!(snap.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(), vec![&ids[2], &ids[1]]);
        assert!(snap.more);
        assert!(!View::open(&path).unwrap().snapshot(3).unwrap().more);
    }

    fn spec(n: usize) -> ChainSpec {
        let contract = serde_json::json!({
            "goal": "do it", "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        });
        let steps: Vec<_> = (0..n)
            .map(|i| serde_json::json!({ "name": format!("step-{i}"), "contract": contract }))
            .collect();
        ChainSpec::parse(
            &serde_json::json!({
                "name": "chain",
                "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
                "steps": steps
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn a_step_task_carries_its_workflow_and_a_failed_chain_says_so() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let s = spec(2);
        let id = db.create_workflow(&s, &serde_json::json!({})).unwrap();
        let view = View::open(&path).unwrap();
        let snap = view.snapshot(10).unwrap();
        assert!(!snap.workflows[0].approved);
        assert_eq!(snap.workflows[0].state(), WorkflowState::Running);
        db.approve_workflow(&id).unwrap();
        let task = db
            .create_step_task(&id, 0, &s.steps[0].contract, &s.digest())
            .unwrap();
        let snap = view.snapshot(10).unwrap();
        assert_eq!(snap.tasks[0].workflow, Some((id.clone(), 0)));
        let w = &snap.workflows[0];
        assert!(w.approved);
        assert_eq!(w.steps[0].task_id.as_ref(), Some(&task));
        assert!(w.steps[0].task_state.is_some() && w.steps[1].task_state.is_none());
        assert_eq!((w.steps[0].model_limit, w.steps[0].tool_limit), (3, 4));
        assert_eq!(w.state(), WorkflowState::Running);
        db.fail_workflow(&id, "boom").unwrap();
        let w = &view.snapshot(10).unwrap().workflows[0];
        assert!(w.failed);
        assert_eq!(w.state(), WorkflowState::Failed { step: 1 });
    }

    #[test]
    fn usage_counts_settled_reserved_and_released_like_the_budget_does() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let id = db
            .create_task(&crate::testing::contract(), &crate::testing::digest())
            .unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            for (status, reserved, settled) in [
                ("Settled", (3, 4), Some((2, 3))),
                ("Reserved", (1, 1), None),
                ("Released", (5, 5), None),
            ] {
                conn.execute(
                    "INSERT INTO usage(task_id, kind, reserved_model_requests, reserved_tool_actions,
                                       settled_model_requests, settled_tool_actions, status)
                     VALUES(?1, 'k', ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id.as_str(), reserved.0, reserved.1,
                        settled.map(|s| s.0), settled.map(|s| s.1), status
                    ],
                )
                .unwrap();
            }
        }
        let task = &View::open(&path).unwrap().snapshot(10).unwrap().tasks[0];
        assert_eq!((task.model_used, task.tool_used), (3, 4)); // 2 settled + 1 reserved; 3 + 1
    }

    #[test]
    fn events_page_in_both_directions_and_payloads_are_capped() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let id = db.create_task(&crate::testing::contract(), &crate::testing::digest()).unwrap();
        for i in 0..5 {
            db.append_audit(&id, "Note", &serde_json::json!({ "i": i, "pad": "x".repeat(100) }))
                .unwrap();
        }
        let view = View::open(&path).unwrap();
        let total = view.event_count(&id).unwrap();
        let latest = view.events(&id, EventPage::Latest, 2).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[1].seq, total, "ascending, ending at the newest");
        let before = view.events(&id, EventPage::Before(latest[0].seq), 2).unwrap();
        assert!(before.iter().all(|e| e.seq < latest[0].seq));
        assert!(before.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(view.events(&id, EventPage::After(total), 10).unwrap().is_empty());
        let (text, truncated) = view.payload(&id, total, 10).unwrap().unwrap();
        assert_eq!(text.chars().count(), 10);
        assert!(truncated);
        assert!(view.payload(&id, total + 1, 10).unwrap().is_none());
    }
}
```

The tests use `crate::testing::{contract, digest}` and `spec(n)`. Add `#[cfg(test)] pub(crate) mod testing` to `view.rs` with:

```rust
pub(crate) fn contract() -> Contract {
    Contract::parse(CONTRACT_JSON).unwrap()
}
pub(crate) fn digest() -> Digest {
    Digest::of(CONTRACT_JSON.as_bytes())
}
const CONTRACT_JSON: &str = r#"{
    "goal": "fix the parser",
    "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
    "profile": "protected", "editable_paths": ["src/"],
    "verification_profile": "parser-checks-v1",
    "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run"],
    "limits": {"model_requests": 10, "max_output_tokens_per_request": 1000, "tool_actions": 1000,
               "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256}
}"#;
```

(copied from `crates/agentos-store/tests/db.rs`). Import `ChainSpec` in the tests module; the event test uses `Db::append_audit` (its real name). If a test fails for a reason the assertion text does not explain (for example `fail_workflow` marks steps differently than assumed), read `crates/agentos-store/src/workflows.rs` and fix the test to the store's real behavior; a test that still contains `todo!` fails the task.

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-store view::`
Expected: FAIL (types not implemented).

- [ ] **Step 3: Implement `View`**

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentos_core::contract::Contract;
use agentos_core::ids::{TaskId, WorkflowId};
use agentos_core::state::TaskState;
use agentos_core::workflow::{ChainSpec, StepStatus, WorkflowState, state_of_steps};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::db::{SCHEMA_VERSION, state_from_str};

impl View {
    pub fn open(path: &Path) -> Result<View, ViewError> {
        if !path.is_file() {
            return Err(ViewError::Missing(path.to_path_buf()));
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "query_only", true)?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found != SCHEMA_VERSION {
            return Err(ViewError::Schema { found, expected: SCHEMA_VERSION });
        }
        Ok(View { conn })
    }

    fn read(&self) -> Result<Transaction<'_>, ViewError> {
        Ok(Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?)
    }
}
```

Task rows query (run inside `snapshot`'s transaction, `LIMIT ?1 + 1` to learn `more`, then truncate):

```sql
SELECT t.id, t.state, t.cancel_requested, t.contract_json, t.created_ts,
       COALESCE(u.m, 0), COALESCE(u.a, 0), ws.workflow_id, ws.idx
FROM tasks t
LEFT JOIN (SELECT task_id,
        SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_model_requests, 0)
                 WHEN status = 'Released' THEN 0 ELSE reserved_model_requests END) AS m,
        SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_tool_actions, 0)
                 WHEN status = 'Released' THEN 0 ELSE reserved_tool_actions END) AS a
      FROM usage GROUP BY task_id) u ON u.task_id = t.id
LEFT JOIN workflow_steps ws ON ws.task_id = t.id
ORDER BY t.created_ts DESC, t.rowid DESC LIMIT ?1
```

Parse `contract_json` into `Contract` for `goal` and `limits.model_requests` / `limits.tool_actions`; parse ids with `serde_json::from_value(Value::String(..))` like `task_id()` in `workflows.rs`; state with `state_from_str`. Any parse failure is `ViewError::Corrupt(..)`; never panic on stored data.

Workflow rows: `SELECT id, spec_json, options_json, approved_ts IS NOT NULL, cancel_requested, created_ts FROM workflows ORDER BY created_ts DESC, rowid DESC LIMIT ?1`, name from `serde_json::from_str::<ChainSpec>(..)?.name`; steps from `workflow_steps` left-joined to `tasks` (state) and the same usage subquery, `ORDER BY idx`, limits from `spec.steps[idx].contract.limits`.

Events: `Latest` is `SELECT seq, type, ts FROM events WHERE task_id=?1 ORDER BY seq DESC LIMIT ?2`, reversed in Rust; `Before(s)` the same with `AND seq < ?3`; `After(s)` is `AND seq > ?3 ORDER BY seq ASC LIMIT ?2`. `event_count` is `SELECT COUNT(*)`. `payload`: `SELECT substr(payload, 1, ?3), length(payload) > ?3 FROM events WHERE task_id=?1 AND seq=?2`.

- [ ] **Step 4: Run to verify they pass**

Run: `docker compose run --rm test cargo test -p agentos-store`
Expected: PASS, including the pre-existing store tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-store
git commit -m "feat: a read-only view of the store for the cockpit"
```

---

### Task 3: `agentos-cockpit` crate: sanitizer and list model

**Files:**
- Create: `crates/agentos-cockpit/Cargo.toml`, `crates/agentos-cockpit/src/lib.rs`, `src/sanitize.rs`, `src/model.rs`
- Modify: `Cargo.toml` (add `ratatui = "0.30.2"` to `[workspace.dependencies]`)

`Cargo.toml` of the crate:

```toml
[package]
name = "agentos-cockpit"
edition.workspace = true
version.workspace = true

[dependencies]
agentos-core = { path = "../agentos-core" }
agentos-store = { path = "../agentos-store" }
ratatui.workspace = true
serde_json.workspace = true
```

`lib.rs`: `pub mod sanitize; pub mod model;` (Tasks 4 and 5 add `app`, `render`, `runtime`).

**Interfaces:**
- Produces:
  - `sanitize::clean(s: &str, max: usize) -> String`: control characters removed (`\n`, `\r`, `\t` become one space), bidi overrides/isolates (U+202A..=U+202E, U+2066..=U+2069) and U+FEFF removed, at most `max` chars, and when cut ends with `…` (so the result has at most `max` chars including the `…`). `max == 0` returns an empty string. Never slices bytes.
  - `model::Key` (`Workflow(WorkflowId)` | `Task(TaskId)`), `model::Item { key: Key, kind: &'static str, id: String, name: String, state: String, progress: String, model_used: u64, model_limit: u64, tool_used: u64, tool_limit: u64, created_ts: i64 }`.
  - `model::items(snapshot: &Snapshot) -> Vec<Item>`: workflows and tasks without a workflow, newest first (`created_ts` desc, ties by id), a task that belongs to a workflow is NOT its own item. A workflow's `state` is `display_label(row.state(), row.approved)`; `progress` is `"{done}/{total}"` where done counts steps whose `task_state` is `Succeeded`; budgets sum its steps (limit and used). A task's `state` is its `TaskState::label()` (plus ` (cancelling)` when `cancel_requested` and not terminal).
  - `model::age(now: i64, ts: i64) -> String`: `"5s"`, `"3m"`, `"2h"`, `"4d"`; a `ts` in the future gives `"0s"`.

- [ ] **Step 1: Write the failing tests**

`sanitize.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn escape_sequences_lose_their_escape() {
        let out = clean("a\x1b[31mred\x1b[0m\x07b", 80);
        assert!(!out.contains('\x1b') && !out.contains('\x07'));
        assert_eq!(out, "a[31mred[0mb");
    }

    #[test]
    fn newlines_and_tabs_become_spaces_and_c1_controls_vanish() {
        assert_eq!(clean("a\nb\tc\r\nd\u{9b}e\u{85}f", 80), "a b c  def");
    }

    #[test]
    fn bidi_overrides_and_bom_vanish() {
        assert_eq!(clean("a\u{202e}b\u{2066}c\u{feff}d", 80), "abcd");
    }

    #[test]
    fn long_text_is_cut_on_a_char_boundary_with_an_ellipsis() {
        assert_eq!(clean("héllo wörld", 5), "héll…");
        assert_eq!(clean("日本語日本語", 4), "日本語…");
        assert_eq!(clean("abc", 3), "abc");
        assert_eq!(clean("abc", 0), "");
        assert_eq!(clean("abc", 1), "…");
    }
}
```

`model.rs` tests: `age` cases (`age(100, 95) == "5s"`, `age(1000, 700) == "5m"`, `age(10_000, 2_800) == "2h"`, `age(400_000, 0) == "4d"`, `age(5, 10) == "0s"`); and `items`: build a `Snapshot` by hand with two standalone tasks and one workflow whose step 0 task is also in `tasks` (with `workflow: Some(..)`); assert the step task is not an item, order is newest first, the workflow row's `progress` is `"1/2"` when step 0 succeeded, its budget sums its steps, a not-approved running workflow shows `READY`, and a `failed` workflow shows the FAILED label.

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-cockpit`
Expected: FAIL (functions missing).

- [ ] **Step 3: Implement**

```rust
pub fn clean(s: &str, max: usize) -> String {
    let kept: Vec<char> = s
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    if kept.len() <= max {
        return kept.into_iter().collect();
    }
    if max == 0 {
        return String::new();
    }
    kept.into_iter().take(max - 1).chain(std::iter::once('…')).collect()
}
```

`age`: seconds `d = now.saturating_sub(ts).max(0)`; `<60` → `"{d}s"`, `<3600` → `"{d/60}m"`, `<86400` → `"{d/3600}h"`, else `"{d/86400}d"`.

`items` as specified above. Use `agentos_core::workflow::display_label`.

- [ ] **Step 4: Run to verify they pass**

Run: `docker compose run --rm test cargo test -p agentos-cockpit && docker compose run --rm test cargo clippy -p agentos-cockpit --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/agentos-cockpit
git commit -m "feat: cockpit sanitizer and list model"
```

---

### Task 4: App state machine and rendering (pure)

**Files:**
- Create: `crates/agentos-cockpit/src/app.rs`, `crates/agentos-cockpit/src/render.rs`
- Modify: `crates/agentos-cockpit/src/lib.rs` (`pub mod app; pub mod render;`)

**Interfaces:**
- Consumes: `model::{items, Item, Key, age}`, `sanitize::clean`, `agentos_store::view::{Snapshot, EventRow, WorkflowRow, Contract via re-export}`.
- Produces:

```rust
pub enum Screen { List, Workflow(WorkflowId), Task(TaskId), Payload { task: TaskId, seq: u64 } }
pub enum Command { None, Quit, Refresh, Watch(Option<TaskId>), Older(TaskId, u64), Payload(TaskId, u64) }
pub struct App {
    pub screen: Screen, pub selected: usize,
    pub snapshot: Option<Snapshot>, pub stale: Option<String>, // Some(reason) after a failed read
    pub events: Vec<EventRow>, pub event_total: u64,
    pub contract: Option<Contract>, pub payload: Option<(String, bool)>,
    pub now: i64,
}
impl App {
    pub fn new() -> App;
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyCode) -> Command;
    pub fn apply_frame(&mut self, frame: Frame);   // Frame: the runtime's message, defined here
    pub fn items(&self) -> Vec<Item>;
}
pub struct Frame { pub snapshot: Result<Snapshot, String>, pub now: i64,
                   pub events: Option<EventUpdate>, pub contract: Option<Contract>,
                   pub payload: Option<(TaskId, u64, Option<(String, bool)>)> }
pub enum EventUpdate { Replace { task: TaskId, rows: Vec<EventRow>, total: u64 },
                       Append { task: TaskId, rows: Vec<EventRow>, total: u64 },
                       Prepend { task: TaskId, rows: Vec<EventRow>, total: u64 } }
pub fn draw(frame: &mut ratatui::Frame, app: &App);   // render.rs
```

Key behavior (`handle_key`; `KeyCode::Char`): `q` or `Esc` on `List` returns `Quit`; on `Workflow`/`Task` goes back to the previous screen (a `Task` opened from a workflow goes back to that workflow; keep `from: Option<WorkflowId>` in the struct) and returns `Watch(None)`; on `Payload` goes back to `Task`. `Up`/`k`, `Down`/`j` move `selected`, clamped to the list length; on `Task` screen at selection 0 with older events available (`events[0].seq > 1`) `Up` returns `Older(task, events[0].seq)`. `Enter` on `List` opens the selected item (`Workflow` or `Task`; a `Task` returns `Watch(Some(id))`), on `Workflow` opens the selected step's task if it has one (returns `Watch(Some(id))`), on `Task` does nothing. `p` on `Task` returns `Payload(task, selected_event.seq)` (and sets nothing until the frame arrives). `r` returns `Refresh`. Every other key returns `None`. There is no code path that returns a command that writes.

Render (all through `sanitize::clean`; widths from `frame.area()`, every width computation saturating; no panic on areas as small as 1x1):
- Header line: `agentos cockpit · {n} items` and, when `stale` is set, ` · STALE: {reason}`; when `snapshot.more`, ` · showing newest {limit}`.
- List: a ratatui `Table`, header `KIND  ID  NAME  STATE  STEPS  MODEL  TOOLS  AGE`, id shortened to its first 8 chars, `MODEL` as `used/limit`, selection highlighted (reversed), an empty home shows the text `no tasks or workflows yet — try 'agentos submit' or 'agentos workflow submit'`.
- Workflow detail: title with the workflow's name and label; one row per step: `#idx name state model used/limit tool used/limit`; a step with no task shows `pending` or `skipped`.
- Task detail: lines for goal, profile, capabilities (`{:?}` of each), limits (model_requests, tool_actions, deadline_seconds) from `contract`, then the event list `seq type age` (header `events (showing {len} of {event_total})`), selected event highlighted.
- Payload: title `event {seq} payload{" (truncated)" if cut}`, the text wrapped by a `Paragraph` with `Wrap { trim: false }`.
- Footer line with the keys valid on this screen (`↑↓ move · Enter open · Esc back · p payload · r refresh · q quit`, only the ones that apply).

- [ ] **Step 1: Write the failing tests**

`app.rs` tests (key handling, no terminal needed): for every `KeyCode` in a table (`Char('a'..='z')`, digits, `Enter`, `Esc`, `Up`, `Down`, `Left`, `Right`, `Tab`, `Delete`, `Backspace`) on every `Screen` the returned `Command` is one of the enum's variants (the enum has no write variant; the test exists to keep it that way and to prove no key panics); `q` on `List` quits; `Esc` on a task opened from a workflow returns to that workflow; `Up` at the first event with older events returns `Older`; `Down` never goes past the last row; `p` on a task with events returns `Payload` with the selected event's seq and with no events returns `None`; applying a failed `Frame` keeps the previous snapshot and sets `stale`; applying a good one clears `stale`; `EventUpdate::Prepend` keeps the selection on the same event (selected index grows by the number of prepended rows).

`render.rs` tests with `ratatui::backend::TestBackend` and a helper:

```rust
fn screen(app: &App, w: u16, h: u16) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
```

Cases: empty home shows the hint; a list with a workflow and a task shows both, the workflow's `1/2`, `READY` for an unapproved one, and does NOT list a workflow's step task separately; the workflow detail shows each step's name and `pending`; the task detail shows goal, `model_requests`, and the event types; `stale` shows `STALE`; a goal `"fix \x1b[31m\nparser \u{202e}ok"` renders with no `\x1b`, no bidi override, and no embedded newline in the row (check the raw buffer symbols contain none of those chars); a payload screen shows `(truncated)` when flagged; drawing at `1x1`, `20x5` and `200x60`, and with 600 items, does not panic; a goal of 300 `日` characters does not panic and fits within the area width.

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-cockpit`
Expected: FAIL (modules missing).

- [ ] **Step 3: Implement `app.rs` and `render.rs`** as specified above. Keep `draw` a pure function of `&App`; keep `handle_key` free of I/O. If the compiler reports a renamed ratatui 0.30.2 item (`Table::new`, `Row`, `Block`, `Paragraph`, `Wrap`, `Style`, `Modifier`, `Layout`/`Constraint`), look it up on docs.rs/ratatui/0.30.2 and keep the behavior.

- [ ] **Step 4: Run to verify they pass**

Run: `docker compose run --rm test cargo test -p agentos-cockpit && docker compose run --rm test cargo clippy -p agentos-cockpit --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-cockpit
git commit -m "feat: cockpit app state and rendering"
```

---

### Task 5: Runtime: worker thread, terminal guard, `run`

**Files:**
- Create: `crates/agentos-cockpit/src/runtime.rs`
- Modify: `crates/agentos-cockpit/src/lib.rs` (`pub mod runtime; pub use runtime::{run, RunError};`)

**Interfaces:**
- Produces: `pub fn run(db_path: &Path) -> Result<(), RunError>` and `pub enum RunError { View(ViewError), Io(std::io::Error) }` (both `From`, `Display`). `run` opens the `View` FIRST (so a missing database or a wrong schema is reported before the terminal is touched), then `ratatui::init()`, and always `ratatui::restore()` on exit (also on panic: `ratatui::init` installs a panic hook that restores; keep it).
- Also produces, for tests: `pub fn serve(view: View, requests: Receiver<Request>, frames: Sender<Frame>, poll: Duration)` where `pub enum Request { Refresh, Watch(Option<TaskId>), Older(TaskId, u64), Payload(TaskId, u64) }` — the worker loop, separated from the terminal so it is testable.

Worker behavior (`serve`): loop on `requests.recv_timeout(poll)`; on timeout or `Refresh` send a `Frame` with a fresh `snapshot(500)`; if a task is watched also send, in the same frame, `EventUpdate::Replace` (first time after `Watch`, latest 200 + `event_count`) or `Append` (rows after the last seen seq) and the task's `contract` (once per `Watch`); `Older(task, before)` sends `Prepend` with `EventPage::Before(before)` of 200; `Payload(task, seq)` sends the payload with `payload(task, seq, 65_536)`; a failed snapshot sends `Err(message)` and the loop keeps going; the loop ends when `requests` is disconnected or `frames.send` fails. The worker is the only user of the `View`.

UI loop (`run`): `crossterm::event::poll(Duration::from_millis(100))`; handle only `KeyEventKind::Press`; translate the `Command` into a `Request` (`Quit` ends the loop); drain `frames.try_recv()` into `app.apply_frame`; redraw each iteration. A terminal resize event just redraws.

Frame rule: a frame's `events` is `Some` only when it has rows to deliver (`Replace` always on the first frame after `Watch`, even with zero rows; `Append`/`Prepend` only with at least one row), so a quiet refresh carries `events: None`.

- [ ] **Step 1: Write the failing tests** (in `runtime.rs` `mod tests`; fixtures as in Task 2: `Contract::parse` JSON, `Db::open`, `Db::append_audit(&id, "Note", &json!({"pad": "x"}))`)

```rust
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    fn start(path: &std::path::Path) -> (Sender<Request>, Receiver<Frame>, std::thread::JoinHandle<()>) {
        let view = View::open(path).unwrap();
        let (rtx, rrx) = mpsc::channel();
        let (ftx, frx) = mpsc::channel();
        let handle = std::thread::spawn(move || serve(view, rrx, ftx, Duration::from_millis(20)));
        (rtx, frx, handle)
    }

    fn frame_with_events(frames: &Receiver<Frame>) -> Frame {
        loop {
            let f = frames.recv_timeout(Duration::from_secs(5)).expect("a frame in time");
            if f.events.is_some() {
                return f;
            }
        }
    }

    fn task_with_notes(dir: &tempfile::TempDir, notes: usize) -> (Db, std::path::PathBuf, TaskId) {
        let path = dir.path().join("agentos.db");
        let db = Db::open(&path).unwrap();
        let c = agentos_core::contract::Contract::parse(CONTRACT_JSON).unwrap();
        let id = db.create_task(&c, &agentos_core::ids::Digest::of(CONTRACT_JSON.as_bytes())).unwrap();
        for i in 0..notes {
            db.append_audit(&id, "Note", &serde_json::json!({ "i": i, "pad": "x" })).unwrap();
        }
        (db, path, id)
    }

    #[test]
    fn the_worker_replaces_then_appends_events_for_the_watched_task() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, id) = task_with_notes(&dir, 5);
        let (requests, frames, worker) = start(&path);
        requests.send(Request::Watch(Some(id.clone()))).unwrap();
        let first = frame_with_events(&frames);
        assert!(first.snapshot.is_ok() && first.contract.is_some());
        let Some(EventUpdate::Replace { rows, total, .. }) = first.events else { panic!("Replace") };
        assert_eq!((rows.len() as u64, total), (6, 6)); // TaskCreated + 5 notes
        db.append_audit(&id, "Note", &serde_json::json!({ "late": true })).unwrap();
        let next = frame_with_events(&frames);
        let Some(EventUpdate::Append { rows, total, .. }) = next.events else { panic!("Append") };
        assert_eq!((rows.len(), total, rows[0].event_type.as_str()), (1, 7, "Note"));
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn older_pages_prepend_and_a_payload_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, id) = task_with_notes(&dir, 250); // 251 events with TaskCreated
        let (requests, frames, worker) = start(&path);
        requests.send(Request::Watch(Some(id.clone()))).unwrap();
        let Some(EventUpdate::Replace { rows, total, .. }) = frame_with_events(&frames).events else { panic!("Replace") };
        assert_eq!((rows.len(), total, rows[0].seq), (200, 251, 52));
        requests.send(Request::Older(id.clone(), rows[0].seq)).unwrap();
        let Some(EventUpdate::Prepend { rows, .. }) = frame_with_events(&frames).events else { panic!("Prepend") };
        assert_eq!((rows.len(), rows.first().map(|r| r.seq), rows.last().map(|r| r.seq)), (51, Some(1), Some(51)));
        requests.send(Request::Payload(id.clone(), 251)).unwrap();
        let payload = loop {
            let f = frames.recv_timeout(Duration::from_secs(5)).unwrap();
            if let Some(p) = f.payload { break p; }
        };
        assert_eq!((payload.0, payload.1), (id, 251));
        let (text, truncated) = payload.2.unwrap();
        assert!(text.contains("pad") && !truncated);
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn closing_the_request_channel_ends_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, _id) = task_with_notes(&dir, 0);
        let (requests, _frames, worker) = start(&path);
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn dropping_the_frame_receiver_ends_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, _id) = task_with_notes(&dir, 0);
        let (_requests, frames, worker) = start(&path);
        drop(frames);
        worker.join().unwrap();
    }
```

`CONTRACT_JSON` is the same constant as in Task 2's `testing` module; copy it into this test module. A hung worker fails the suite by timeout, so run these with `timeout 120`.

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-cockpit runtime`
Expected: FAIL.

- [ ] **Step 3: Implement** `serve`, `run`, `RunError` as specified. Keep `run` small: open the view, spawn `serve` on a `std::thread`, init the terminal, loop, restore. Join the worker after dropping the request sender.

- [ ] **Step 4: Run to verify they pass**

Run: `docker compose run --rm test cargo test -p agentos-cockpit && docker compose run --rm test cargo clippy -p agentos-cockpit --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-cockpit
git commit -m "feat: cockpit worker thread and terminal loop"
```

---

### Task 6: `agentos cockpit`, README and spec wording

**Files:**
- Modify: `crates/agentos-cli/Cargo.toml` (add `agentos-cockpit = { path = "../agentos-cockpit" }`)
- Modify: `crates/agentos-cli/src/args.rs` (a `Cockpit` variant of `Command`, doc comment "Read-only overview of every task and workflow")
- Create: `crates/agentos-cli/src/commands/cockpit.rs`; modify `commands/mod.rs` (`mod cockpit;`, dispatch arm)
- Modify: `crates/agentos-cli/tests/cli.rs` (two tests)
- Modify: `README.md` (a "Cockpit" section), `docs/superpowers/specs/2026-10-13-read-only-cockpit-design.md` (the two rulings at the top of this plan)

**Behavior:** `cockpit::cockpit(home: &Home) -> Result<(), CliError>`: if `!std::io::IsTerminal::is_terminal(&std::io::stdout())` return `CliError::usage("agentos cockpit needs a terminal on stdout")` (exit 2); the database path is `home.root.join("agentos.db")`; call `agentos_cockpit::run(&path)` and map `RunError::View(ViewError::Missing(p))` to `CliError::usage(format!("no agent-os database at {}", p.display()))` (exit 2), `ViewError::Schema{..}` to a usage error carrying the view's message, other errors to `CliError::other`. It must NOT call `home.open()` (that creates directories). `cockpit` takes no task argument, so `task_arg` needs no change; the `--worker` check in `run` only fires for task commands.

- [ ] **Step 1: Write the failing tests** (in `crates/agentos-cli/tests/cli.rs`, near the other command tests; `Cli::bare()` gives a scratch home, `cli.cmd(&[..])` runs the binary with stdout piped, so it is not a terminal)

```rust
#[test]
fn cockpit_needs_a_terminal() {
    let cli = Cli::bare();
    cli.cmd(&["cockpit"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs a terminal"));
}

#[test]
fn cockpit_with_no_database_is_refused_and_creates_nothing() {
    // stdout is not a terminal in tests, so the terminal check fires first; the missing
    // database case is covered where the terminal is available: call the library entry
    // point directly.
    let cli = Cli::bare();
    let path = cli.home().join("agentos.db");
    let err = agentos_cockpit::run(&path).unwrap_err();
    assert!(err.to_string().contains("no agent-os database"));
    assert!(!cli.home().exists(), "nothing was created");
}
```

Add `agentos-cockpit` to `crates/agentos-cli/Cargo.toml` `[dev-dependencies]` too for the second test (it is already a normal dependency; no change needed if it is listed there once).

- [ ] **Step 2: Run to verify they fail**

Run: `docker compose run --rm test cargo test -p agentos-cli --test cli cockpit`
Expected: FAIL (unknown subcommand `cockpit`).

- [ ] **Step 3: Implement** the command, the `Cockpit` arm in `args.rs` / `commands/mod.rs`, and the docs:
  - README "Cockpit" section: how to start (`agentos cockpit`), the keys (`↑↓/jk`, `Enter`, `Esc`/`q`, `p`, `r`), what is shown (list, workflow steps, task terms and events), that it is read-only (opened read-only, no lock, never migrates, never creates), that it refuses a database of another schema version, that a payload is shown only on `p`, as stored, and may contain task content, and that a stale header means the last read failed.
  - Spec: replace `Db::open_read_only` wording with `View::open` and the two rulings from this plan's header (separate `View` type; events: latest 200 plus older pages on reaching the top).

- [ ] **Step 4: Full verification**

Run, each separately in the foreground:
`docker compose run --rm test cargo fmt --check`
`docker compose run --rm test cargo clippy --workspace --all-targets -- -D warnings`
`docker compose run --rm test cargo test --workspace`
Expected: all clean/PASS, no new warnings.

- [ ] **Step 5: Manual smoke** (a terminal is needed, so the harness cannot do it): create a home with the CLI test fixtures' commands (`agentos submit` of the parser fixture with `--fake-agent-patch`), then run `docker compose run --rm test cargo run -q -p agentos-cli -- --home <that home> cockpit` from an interactive terminal (`docker compose run` allocates one) and check the list, Enter, `p`, Esc and `q`. If no interactive terminal is available, say in the report that this step was NOT run.

- [ ] **Step 6: Commit**

```bash
git add -A crates README.md docs
git commit -m "feat: agentos cockpit, a read-only overview of every task and workflow"
```
