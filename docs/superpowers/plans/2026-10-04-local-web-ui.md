# Local Web UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run and inspect Agent OS tasks locally in a browser, approve their recorded contracts, review patches/final verification, and download authorized exports.

**Architecture:** Add shared task operations inside `agentos-cli`, with thin CLI and Axum/Askama adapters. Read views use bounded store queries and the engine's pure export projection; mutations keep existing broker, reducer and driver-lock behavior. An owned runner thread progresses tasks independently of HTTP requests.

**Tech Stack:** Rust/Tokio, Axum 0.8.9, Askama 0.16.1, locally vendored HTMX 2.0.11, tar 0.4.46; Tower 0.5.3 for router tests; Playwright Python 1.63.0 for browser tests in a virtual environment created by the existing pyenv Python.

**Spec:** [Approved local web UI design](../specs/2026-10-04-local-web-ui-design.md).

**Base:** `1564c08`, existing isolated `codex/v01-completion` worktree. This plan is a separate milestone from the remaining v0.1 packages. No product code has been implemented by this planning commit.

## Global Constraints

- One local owner, one Agent OS home, and the existing one-driver-per-home rule.
- `agentos ui --port 8080` binds only `127.0.0.1`; `--port 0` supports ephemeral test ports.
- Closing a tab does not stop a run; restarting the server offers explicit recovery and never auto-approves/resumes tasks.
- Normal GETs must leave task, effect, usage, capability and journal rows unchanged.
- A READY task is created without issuing capabilities or sending model requests. Approve and start is a separate deliberate POST bound to its task ID and reviewed contract digest.
- Only `SUCCEEDED` with matching final/verified digests and accepted evidence earns a verified-result label.
- Contract/form body 256 KiB; event summary/log preview 64 KiB; patch preview 256 KiB; at most 50 tasks and 100 events per page; result/export payload total 128 MiB.
- Enforce result byte budgets before allocating/reading full blobs, not after collection.
- Blocking-query concurrency is four; admit at most one driver run per home, without a hidden queue.
- Download IDs are session/task scoped; expire archives after ten minutes and bound the cache to eight archives.
- At most 256 submission-nonce entries per session; expire unused/completed entries after 15 minutes and never evict pending entries.
- Launch token: cryptographically random 32 bytes in the URL fragment. HttpOnly, SameSite=Strict cookie; exact Host and same-origin POST Origin; per-session CSRF after bootstrap; no CORS grants; no-store responses; self-only CSP; escaped HTML.
- Provider keys and full capability handles never become application view fields. No CDN, general filesystem browser, shell, automatic patch application, GC/registry controls, TUI, accounts, remote workers, new scheduler, WebSocket/SSE, or journal-schema upgrade.
- Docker Compose for builds/tests; Python through the pinned pyenv interpreter and a project virtual environment; no global pip install and no Ruby runtime added.
- Preserve CLI JSON shapes, exit codes, worker/model provenance, cancellation/recovery behavior and unchanged patch/evidence bytes. Never add co-author commit trailers.
- Required code gates are formatting, strict Clippy, full host and full fake-jail suites via `scripts/check.sh`, plus Compose browser acceptance. Offline passes do not establish real provider/KVM acceptance.

## Review Focus

1. Timestamp ties and changing state filters: cursor paging must neither duplicate nor omit a task, and a cursor from another filter must be rejected (Task 1).
2. Multibyte and malformed text in goals/diffs/logs: previews must not panic or become executable HTML; full export bytes must remain unchanged (Tasks 2 and 4).
3. A stale browser after server restart or a lost Create response: old session/nonces must fail, and a live duplicate must resolve to the already-created task (Tasks 3 and 9).
4. Staged inputs changed between review and start: UI approval must refuse before issuing capabilities; existing CLI tamper/recovery behavior must remain intact (Tasks 7 and 9).
5. Revocation or expiry after export creation and interrupted server lifetime: cached downloads must obey current authority and only owned cache data may be released (Tasks 5 and 10).

## File map and responsibilities

| Files | Responsibility |
| --- | --- |
| `agentos-store/src/read.rs`, `agentos-store/tests/read.rs` | Parameterized pagination and byte-bounded journal/metadata reads |
| `agentos-store/src/blob.rs` | Bounded, integrity-checked blob reads beside the existing unbounded CLI API |
| `agentos-engine/src/export.rs`, `agentos-engine/tests/export.rs` | Shared pure bundle contents and bounded collection |
| `agentos-engine/src/journal.rs` | Patch lookup from already-read events; no second unbounded scan |
| `agentos-cli/src/app/{mod,error,types,queries,export,submission,control,runner}.rs` | Shared application results and existing task operations |
| `agentos-cli/src/home/provenance.rs`, `agentos-cli/src/home.rs` | Shared parsing of first Submitted metadata, nonmutating driver observation and cloneable configuration |
| `agentos-cli/src/ui/{mod,session,error,routes,views,downloads,forms}.rs` | Loopback server, authenticated routes, HTML views, scoped caches and form admission |
| `agentos-cli/templates/ui/*.html`, `agentos-cli/assets/ui/*` | Escaped layouts/fragments, local scripts/CSS and HTMX provenance |
| `agentos-cli/tests/ui.rs`, `agentos-cli/tests/common/ui.rs` | Real-process HTTP tests using the existing CLI as an oracle |
| `tests/ui/{common,test_review,test_workflow,test_recovery}.py` | Headless browser acceptance on disposable homes |
| `Dockerfile` opt-in ui-test stage, `runtime/ui-browser-requirements.txt`, `scripts/test-ui.sh` | Browser runtime isolated from required Rust test image |
| `compose.yaml`, `.github/workflows/ui.yml`, `README.md`, `docs/ui.md` | Local launch, repeatable browser checks and documented lifetime/limits |

All crate paths in the table are below `crates/`. Keep each file focused; `ui/routes.rs`
wires handlers and delegates work instead of becoming a second controller. New unit
tests live beside internal application/UI modules; integration tests operate the binary
and need no public API for the private `Home` type.

## Execution setup and dependency evidence

Reuse the existing worktree if it still matches this base; inspect its status first and
preserve unrelated changes. Check the approved spec and plan before editing code. Run
the existing four offline gates once as the baseline and retain logs under `/tmp`.

Checked 2026-10-04: [Axum](https://docs.rs/axum/latest/axum/) 0.8.9,
[Askama](https://docs.rs/askama/latest/askama/) 0.16.1,
[HTMX](https://htmx.org/docs/) 2.0.11, [tar](https://docs.rs/tar/latest/tar/) 0.4.46,
[Tower](https://docs.rs/tower/latest/tower/) 0.5.3 and
[Playwright](https://pypi.org/project/playwright/) 1.63.0. Recheck at implementation.
Use the [official browser installation guidance](https://playwright.dev/python/docs/docker)
to build a derived test image rather than replacing the project's pyenv interpreter.
There is currently no uv/Poetry/Pipenv/Conda/requirements manager in this Rust workspace;
use venv plus pinned requirements for the Python test dependency.

In commands below, run from the worktree with `COMPOSE_PROJECT_NAME=agent-os` or another
dedicated project. New tests must fail for missing/wrong behavior before implementation;
a fixture/import/compilation error is not evidence of a behavioral regression. Fix test
setup and supply minimal new API/type signatures where needed, then observe the intended
assertion failure before implementing behavior. Use specific files in `git add`, followed by a
focused commit without co-author trailers. Run dependent tasks sequentially.

## Task 1: Bounded store queries and blob reads

**Files:** Create `crates/agentos-store/src/read.rs`, `crates/agentos-store/tests/read.rs`; modify `crates/agentos-store/src/{lib,db,blob}.rs`; add blob tests beside the implementation.

**Interfaces produced:**

```rust
// agentos_store::read; all fields owned and Serialize where used as views.
pub struct TaskCursor { pub created_ts: i64, pub id: TaskId, pub filter: Option<TaskState> }
pub struct TaskListRow {
    pub task: Task, pub goal: String, pub repository_source: String,
    pub created_ts: i64, pub text_truncated: bool,
}
pub struct TaskPage { pub rows: Vec<TaskListRow>, pub next: Option<TaskCursor> }
pub struct EventHeader { pub seq: u64, pub event_type: String, pub ts: i64, pub truncated: bool }
pub struct EventPage { pub rows: Vec<EventHeader>, pub last_seq: u64, pub has_more: bool }
// New inherent Db methods, returning agentos_store::db::Result:
// tasks_page(filter: Option<TaskState>, before: Option<&TaskCursor>, limit: usize) -> Result<TaskPage>
// event_headers(task: &TaskId, after: u64, limit: usize) -> Result<EventPage>
// events_bounded(task: &TaskId, max_bytes: u64) -> Result<Vec<StoredEvent>>
// first_event_bounded(task: &TaskId, kind: &str, max_bytes: Option<u64>) -> Result<Option<StoredEvent>>
// contract_bounded(task: &TaskId, max_bytes: u64) -> Result<Contract>
// effect_bounded(effect: &EffectId, max_bytes: u64) -> Result<EffectRecord>
// outstanding_effects_bounded(task: &TaskId, max_bytes: u64) -> Result<Vec<EffectRecord>>
// BlobStore::get_bounded(digest: &Digest, max_bytes: u64) -> Result<Vec<u8>, BlobReadError>
#[derive(Debug)]
pub enum BlobReadError { Io(std::io::Error), TooLarge { limit: u64 } }
```

Add `DbError::InvalidQuery(String)` and `DbError::ReadLimit { limit: u64 }`; existing
methods keep their behavior. Use `Db::read()` transactions available to sibling modules.
`TaskCursor.filter` must agree with the requested filter. Reject limits outside 1..=50
and 1..=100 respectively and `after` values outside SQLite's signed integer range.

- [ ] **Write the store/blob regressions.** Use the existing store contract test fixture;
  create three tasks, set their `created_ts` to the same value using a connection to the
  disposable fixture DB, and derive the expected descending ID order. The core assertions:

```rust
let first = db.tasks_page(None, None, 2).unwrap();
let second = db.tasks_page(None, first.next.as_ref(), 2).unwrap();
let ids: Vec<_> = first.rows.iter().chain(&second.rows).map(|r| r.task.id.clone()).collect();
assert_eq!(ids, expected_descending_ids);
assert!(second.next.is_none());
assert!(matches!(db.tasks_page(Some(TaskState::Ready), first.next.as_ref(), 2),
    Err(DbError::InvalidQuery(_))));
let digest = blobs.put(b"123456789").unwrap();
assert!(matches!(blobs.get_bounded(&digest, 8), Err(BlobReadError::TooLarge { limit: 8 })));
assert_eq!(blobs.get_bounded(&digest, 9).unwrap(), b"123456789");
```

  Add event-sequence paging, unknown task, huge payload, invalid limit/cursor, Unicode
  truncation, regular-file refusal and a corrupt blob within the byte limit. A sparse
  oversized object with a deliberately wrong digest must yield TooLarge before integrity
  hashing, proving that the reader did not allocate the object first.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-store --test read --locked` and `docker compose run --rm test cargo test -p agentos-store --lib --locked bounded_blob`.
- [ ] **Implement parameterized read queries.** Use existing state decoding; order by
  `created_ts DESC, id DESC`, fetch `limit + 1`, and emit a cursor only when another row
  exists. SQL cursor predicate:

```sql
WHERE (?1 IS NULL OR state = ?1)
  AND (?2 IS NULL OR created_ts < ?2 OR (created_ts = ?2 AND id < ?3))
ORDER BY created_ts DESC, id DESC LIMIT ?4
```

  Extract only bounded goal/repository labels for the list (4096 UTF-8 bytes each, marked
  shortened), and type/timestamp/sequence for the timeline. Do not select full event
  payloads or decode the full contract to construct a TaskListRow. For a stored contract
  above 256 KiB, use a short unavailable-goal label and text_truncated=true before any
  JSON field extraction; CASE guards prevent parsing oversized legacy contract text.
  Measure SQLite BLOB byte lengths inside the same read
  transaction before decoding bounded contracts, events or effect kinds. Budget the
  aggregate event/effect data, not merely each row. Preserve current first-Submitted
  behavior. New read APIs check existence with a metadata-only query rather than calling
  `load_task`, which decodes the entire contract. Outstanding effects keep existing order
  but reject an aggregate kind-payload budget before collecting/decoding their rows.
- [ ] **Implement bounded blob reads.** Open once with no-follow/nonblocking flags,
  require a regular file, reject metadata length above the limit, then read at most
  `limit + 1` bytes from that descriptor to detect growth. Hash only complete accepted
  bytes. Keep `get()` unchanged for compatibility.
- [ ] **Run GREEN and commit.** Run the two focused commands plus the existing store
  suite; require no task/event writes from queries. Commit `feat: add bounded task and journal read views`.

## Task 2: Shared status and pure result collection

**Files:** Create `crates/agentos-cli/src/app/{mod,error,types,queries}.rs` and `crates/agentos-cli/src/home/provenance.rs`; modify `crates/agentos-cli/src/{lib,home}.rs`, `crates/agentos-cli/src/commands/inspect.rs`, `crates/agentos-engine/src/{export,journal}.rs`; test in `crates/agentos-engine/tests/export.rs`, app query unit tests and `crates/agentos-cli/tests/cli.rs`.

**Interfaces:** Consume Task 1 read types. Produce these internal application contracts:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppErrorKind { Invalid, Forbidden, NotFound, Conflict, TooLarge, Gone, Unavailable }
#[derive(Debug)]
pub(crate) struct AppError { pub kind: AppErrorKind, pub cli_code: i32, pub message: String }
pub(crate) type AppResult<T> = Result<T, AppError>;
pub(crate) struct GuestImageRef { pub id: String, pub digest: Digest }
pub(crate) struct OutstandingEffect {
    pub effect_id: EffectId, pub kind: String, pub state: EffectState, pub lease_generation: u64,
}
pub(crate) struct JobSummary {
    pub effect_id: EffectId, pub attempt_id: AttemptId, pub lease_generation: u64,
    pub state: Option<JobState>, pub alive: bool, pub receipt: bool,
}
pub(crate) struct CapabilitySummary {
    pub operation: Capability, pub handle_prefix: String, pub revoked: bool, pub expires_ts: Option<i64>,
}
pub(crate) struct StatusView {
    pub task_id: TaskId, pub state: String, pub step: u32, pub cancel_requested: bool,
    pub workspace_digest: Digest, pub verified_digest: Option<Digest>, pub actions_used: u32,
    pub usage: UsageSummary, pub outstanding_effects: Vec<OutstandingEffect>,
    pub jobs: Vec<JobSummary>, pub capabilities: Vec<CapabilitySummary>,
    pub worker: String, pub model: String, pub model_policy_version: u32,
    pub model_limits_version: u32, pub model_endpoint: Option<String>,
    pub guest_image: Option<GuestImageRef>, pub jailed: Option<bool>,
}
pub(crate) struct TaskSummary { pub row: TaskListRow, pub worker: String, pub model: String }
pub(crate) struct TaskListView { pub rows: Vec<TaskSummary>, pub next: Option<TaskCursor> }
pub(crate) struct TaskDetail {
    pub status: StatusView, pub contract: Contract, pub contract_digest: Digest,
    pub repository_digest: Option<Digest>, pub profile_digest: Option<Digest>,
}
// queries::status(home: &Home, task: &TaskId, max_bytes: Option<u64>) -> AppResult<StatusView>
// queries::tasks(home: &Home, filter: Option<TaskState>, cursor: Option<&TaskCursor>) -> AppResult<TaskListView>
// queries::detail(home: &Home, task: &TaskId) -> AppResult<TaskDetail>
// queries::events(home: &Home, task: &TaskId, after: u64) -> AppResult<EventPage>
// queries::review(home: &Home, task: &TaskId) -> AppResult<ReviewContents>
// engine::export shared pure projection:
pub struct ReviewContents {
    pub manifest: Manifest, pub patch_diff: Vec<u8>, pub patches: Vec<(String, Vec<u8>)>,
    pub model_files: Vec<(String, Vec<u8>)>, pub evidence: BTreeMap<Digest, Vec<u8>>,
}
// collect_review(db: &Db, blobs: &BlobStore, task: &TaskId, max_bytes: Option<u64>) -> Result<ReviewContents, ExportError>
```

Derive Serialize for status types, skipping only absent `guest_image` and `jailed`;
all other null/enum/string fields must match current CLI output. CLI status uses `None`
for compatibility; web reads use `Some(128 * 1024 * 1024)`. For bounded metadata parsing,
factor first-Submitted parsing from Home into `home/provenance.rs` and reuse it from both
callers rather than calling old whole-journal lookups. The list adds recorded model/worker
labels from only the first Submitted event. Errors retain existing CLI message/code while
carrying a typed HTTP category; never classify by substring matching error messages.
Every bounded detail/status/review query first uses contract_bounded with 256 KiB before
any legacy `task`, `usage_summary`, `grants` or pure `check` call that internally parses
the contract. Web status uses outstanding_effects_bounded; the CLI keeps its existing
unbounded adapter. Include oversized legacy contracts/effect metadata in query tests.

- [ ] **Write the pure-review test in the existing engine fixture.** Reuse `Env`,
  `succeeded` and `export` already defined in `tests/export.rs`:

```rust
#[tokio::test]
async fn pure_review_keeps_journal_and_matches_export_bytes() {
    let env = Env::new(10);
    succeeded(&env).await;
    let before = env.db.events(&env.task).unwrap();
    let review = collect_review(&env.db, &env.blobs, &env.task, Some(128 * 1024 * 1024)).unwrap();
    assert_eq!(env.db.events(&env.task).unwrap(), before);
    let root = tempfile::tempdir().unwrap();
    export(&env, &env.task, &root.path().join("bundle")).unwrap();
    assert_eq!(review.patch_diff, fs::read(root.path().join("bundle/patch.diff")).unwrap());
    for (digest, bytes) in review.evidence {
        assert_eq!(bytes, fs::read(root.path().join(format!("bundle/evidence/{digest}.json"))).unwrap());
    }
}
```

  Add tiny aggregate budget, a large journaled patch with no blob, corrupt/missing
  evidence, empty patch, failed/cancelled task and a passed result not accepted for the
  final workspace. In CLI tests freeze all existing status keys, enum casing, optional
  guest metadata, endpoint/model policy versions and unchanged event-line output.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-engine --test export --locked pure_review`.
- [ ] **Factor the collector and budget path.** Rename private Contents to ReviewContents;
  reuse it in `export_bundle` without changing its audit. Add `ExportError::ReadLimit`.
  Read bounded contract/events/effect metadata through Task 1. Charge retained bytes
  before copying each patch/model/evidence object; keep a per-collection remaining budget.
  Extract `journal::patch_from_events(events: &[StoredEvent], digest: &Digest)` to avoid
  `journaled_patch` rereading the unbounded journal in a bounded request. Existing replay
  callers keep their public behavior. No blob may escape the remaining-byte reader.
- [ ] **Extract shared queries and keep adapters compatible.** Web review first calls
  pure `Db::check(task, Capability::ArtifactExport, &Resource::Task)`. Add a nonmutating
  `Home::driver_status() -> AppResult<Option<TaskId>>` using a read-only lock descriptor:
  an acquired observation lock is immediately released without truncation; absent means
  idle and stale contents are not authority. Keep CLI events returning full original
  events, and expose paged headers as a separate web query. Detail uses a bounded
  256-KiB stored contract plus recorded digests; list/status/detail are inspection,
  whereas patch/evidence content requires the pure export-capability check.
- [ ] **Run GREEN and commit.** Run engine export, store, app query and full CLI suites;
  commit `refactor: share bounded task status and pure result views`.

## Task 3: Loopback server and local session

**Files:** Create `crates/agentos-cli/src/ui/{mod,session,error,routes}.rs`, `crates/agentos-cli/templates/ui/{bootstrap,tasks,error}.html`, `crates/agentos-cli/assets/ui/bootstrap.js`, `crates/agentos-cli/tests/{ui.rs,common/ui.rs}`; modify `Cargo.toml`, `Cargo.lock`, `crates/agentos-cli/Cargo.toml`, CLI `args.rs`, `lib.rs`, `commands/mod.rs`, and derive Clone for `Home` configuration.

**Interfaces:** Consume Task 2 queries/errors. Produce `UiConfig { port: u16 }`,
`SessionId(String)` and `UiState` containing `Arc<Home>`, four query permits, session
registry and actual listener authority. `ui::serve(home: Home, config: UiConfig)` binds
loopback and blocks until shutdown. `ui::router(state: Arc<UiState>) -> axum::Router`
is independently request-testable. Session module provides `bootstrap(token, origin)`,
cookie validation and CSRF validation without reading task data.

HTTP fixture contract in `tests/common/ui.rs`: `UiFixture::new()` owns TempDir and the
existing parser inputs; `start()` returns `UiServer` with `url`, `launch_url`, child
process and a redacted stderr collector; `UiServer::session()` returns a reqwest client
with the session cookie and CSRF token. Fixtures scrub real provider/worker credentials
and use bounded startup/request deadlines. `stop()`/Drop terminate only their own child.
Use `env!("CARGO_BIN_EXE_agentos")` in this integration helper. Read the single startup
JSON record `{ "listening": "http://127.0.0.1:PORT", "launch_url": "...#TOKEN" }`;
this one launch credential is intentional stdout, never a request/trace log.

- [ ] **Write the HTTP boundary tests using actual binary requests.** Core test:

```rust
#[tokio::test]
async fn local_session_protects_task_data() {
    let fixture = UiFixture::new();
    let server = fixture.start();
    let anonymous = reqwest::Client::new();
    assert_eq!(anonymous.get(format!("{}/tasks", server.url)).send().await.unwrap().status(), 403);
    let session = server.session().await;
    assert_eq!(session.client.get(format!("{}/tasks", server.url)).send().await.unwrap().status(), 200);
    assert_eq!(session.client.get(format!("{}/tasks", server.url)).header("Host", "evil.invalid")
        .send().await.unwrap().status(), 403);
}
```

  `UiSession` exposes `client: reqwest::Client` and `csrf: String`. Add forged/missing
  Origin, incorrect launch token, missing CSRF after bootstrap, old cookie after restart,
  percent-encoded traversal IDs/assets, unknown UUID, 256-KiB body limit and provider-key
  sentinel tests. Rejected operations must leave journal and filesystem footprint intact.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-cli --test ui --locked local_session`.
- [ ] **Add pinned routing/template dependencies and feature flags.** Use Axum 0.8.9,
  Askama 0.16.1, `getrandom.workspace = true`, `tower = { version = "0.5.3", features = ["util"] }`
  in dev dependencies, and Tokio `net`, `sync`, `signal`, `io-util` features only as needed.
  Add `reqwest` cookie support only for integration tests. Update the lockfile through
  Compose; do not hand-edit checksums or disable `--locked` in subsequent checks.
- [ ] **Implement bootstrap and guarded task-list rendering.** Generate 32 random bytes
  per launch/session/CSRF, encode with existing base64 URL-safe no-pad, and never derive
  them from task IDs. Middleware verifies actual Host, session and POST Origin/CSRF before
  dispatch. Assets are an allowlist with explicit content types. Bootstrap JS:

```javascript
const token = location.hash.slice(1);
history.replaceState(null, "", "/");
const response = await fetch("/session", {
  method: "POST", credentials: "same-origin",
  headers: {"Content-Type": "application/json"}, body: JSON.stringify({token})
});
if (response.ok) location.replace("/tasks");
else document.querySelector("[data-error]").textContent = "Open the launch link printed by agentos ui.";
```

  Serve it as a local module; permit no inline script/eval in CSP. Render a real paged
  task table now, using Task 2, rather than empty success routes. Convert AppErrorKind to
  the spec's status codes; use the same escaped error panel for full pages/fragments.
  Semaphore admission precedes spawn_blocking; requests do not share Db connections.
- [ ] **Run GREEN and commit.** Run UI boundary tests, CLI argument/help tests and Clippy;
  commit `feat: serve a protected local task dashboard`.

## Task 4: Detail, patch, verification and event views

**Files:** Create `crates/agentos-cli/src/ui/views.rs`, templates `ui/{layout,task,status,result,events}.html`, assets `ui/{app.js,app.css,htmx.min.js,htmx.LICENSE,htmx.sha256}`; extend `ui/routes.rs` and `tests/ui.rs`.

**Interfaces:** Consume StatusView, EventPage and ReviewContents. Produce
`Preview { text: String, bytes: usize, truncated: bool }` and
`preview(bytes: &[u8], limit: usize) -> Preview`, plus
`ResultView { manifest: Manifest, patch: Preview, verified_final: bool, evidence: Vec<Preview> }`.
`verified_final` requires SUCCEEDED, equal nonempty final/verified digests and accepted
verification evidence for that digest; use manifest acceptance flags from Task 2.

- [ ] **Write view and route regressions.** The multibyte boundary test is explicit:

```rust
#[test]
fn preview_and_html_do_not_split_or_execute_untrusted_text() {
    let shown = preview("é<script>alert(1)</script>".as_bytes(), 3);
    assert!(shown.truncated);
    assert_eq!(shown.text, "é<");
    assert_eq!(shown.bytes, 3);
    let html = render_patch(&preview(b"<script>alert(1)</script>", 256 * 1024)).unwrap();
    assert!(!html.contains("<script>"));
    assert!(html.contains("alert(1)"));
}
```

  Define `render_patch(preview: &Preview) -> Result<String, askama::Error>` in views.rs.
  Add malformed UTF-8 display (lossy text, preserved raw export), HTML-bearing goals/logs,
  duplicate event pages, state-label cases, integrity/413 failures and a revoked export
  grant. Snapshot actual DB rows/event sequence around repeated GETs; neither pure checks
  nor lock observation may append/reset anything.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-cli --lib --locked preview_and_html` and the new view route tests in `--test ui`.
- [ ] **Implement the escaped templates and byte previews.** Apply the frontend-design
  skill during this implementation step for the real dashboard layout and interaction.
  Shorten valid UTF-8 at a
  codepoint boundary; for invalid UTF-8 use lossy decoding of the bounded byte slice.
  Show source byte length and an explicit preview-truncated notice. Never put `|safe`
  on repository/journal/evidence text. Use semantic tabs, visible focus, text status labels,
  line-prefix diff styling and responsive code panels; no charts or decorative metrics.
- [ ] **Vendor HTMX and implement polling/error behavior.** Verify the downloaded
  2.0.11 asset against its documented SHA-384 integrity value and record local SHA-256
  plus license/source. Local app.js configures `allowEval=false`, no injected inline
  indicator styles, authenticated CSRF headers and fragment error handling. Visibility-
  aware polling every two seconds preserves previous content on failure, marks stale
  time, paginates events until caught up, deduplicates sequence numbers, and stops when
  terminal with no pending cancellation/effects. A task details refresh updates action
  availability from durable state rather than optimistic client labels.
- [ ] **Run GREEN and commit.** Run view, HTTP and engine projection tests; commit
  `feat: review task patches and final verification in the browser`.

## Task 5: Authorized archive export and download cache

**Files:** Create `crates/agentos-cli/src/app/export.rs`, `crates/agentos-cli/src/ui/downloads.rs`, export fragment template; modify CLI/engine export adapters, UI routes/state, Cargo manifests/lockfile and HTTP tests.

**Interfaces:** Consume ReviewContents, AppErrorKind and SessionId. Produce
`app::export::write(home: &Home, task: &TaskId, out: &Path, max_bytes: Option<u64>) -> AppResult<Manifest>`;
the CLI passes None and the UI passes 128 MiB. Engine adds
`export_bundle_bounded(db, blobs, task, out, max_bytes: Option<u64>) -> Result<Manifest, ExportError>`
and keeps `export_bundle` as the None-budget adapter. Download cache produces
`ArchiveTicket { id: DownloadId, task: TaskId, bytes: u64 }`, where `DownloadId(String)`
is an opaque signed session/task/expiry token. `DownloadCache::create(session, home, task)`
returns a ticket; `open(session, id, home)` returns an independently opened archive file
after a pure current-authority check. An injectable monotonic elapsed-time clock controls
ten-minute expiry; the test clock can advance without sleeping.
Implement the clock as an injected `Arc<dyn Fn() -> Duration + Send + Sync>`; production
uses a captured Instant. Tests define `ManualClock(Arc<AtomicU64>)` with `advance(Duration)`
and pass a closure reading its seconds. Cache open returns an OwnedArchiveReader containing
a fresh Tokio File, content length and an Arc to its TempDir; its AsyncRead implementation
delegates to the file while retaining the owner through stream completion.

- [ ] **Write archive/cache tests and HTTP export parity.** With two authenticated
  sessions over a terminal fixture, assert:

```rust
let ticket = cache.create(&session_a, &home, &task).unwrap();
let before = home.open().unwrap().db.events(&task).unwrap();
let archive = cache.open(&session_a, &ticket.id, &home).unwrap();
assert_eq!(home.open().unwrap().db.events(&task).unwrap(), before);
assert!(matches!(cache.open(&session_b, &ticket.id, &home),
    Err(AppError { kind: AppErrorKind::Forbidden, .. })));
drop(archive);
clock.advance(std::time::Duration::from_secs(601));
assert!(matches!(cache.open(&session_a, &ticket.id, &home),
    Err(AppError { kind: AppErrorKind::Gone, .. })));
```

  Unit-test helpers construct a terminal task via a CLI-seeded integration fixture or
  an engine fixture; do not synthesize SUCCEEDED by writing lifecycle SQL. Compare every
  archive entry's path and bytes with a CLI export, except manifest's audit-dependent
  `generated_events` count. Repeat patch/evidence parity after `agentos gc` over the
  disposable fixture home, reusing the existing GC preservation fixture pattern.
  Add revoked grant after caching, corrupt blob, over-budget
  export, eviction on ninth archive, tampered/unknown ID, independent simultaneous readers,
  and sentinel files outside the owned cache. Failed export must not append `Exported`.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-cli --test ui --locked export_download` and cache unit tests filtered by `download_cache`.
- [ ] **Factor export mutation and archive creation.** Keep the existing broker grant/
  denial audit before export, then call the bounded collector/writer. Web calls perform
  the 256-KiB contract guard before entering legacy authorization. Reserve a cache
  slot before work, own a TempDir, package only the generated regular bundle files with
  tar 0.4.46, sorted names and deterministic headers. Never extract anything. Add
  `blake3.workspace = true` for keyed download-token integrity and `tokio-util = { version
  = "0.7.19", features = ["io"] }` for streaming; both were checked against primary
  [BLAKE3](https://docs.rs/blake3/latest/blake3/) and
  [Tokio utilities](https://docs.rs/tokio-util/latest/tokio_util/) docs on 2026-10-04.
- [ ] **Implement bounded cache and stream ownership.** Sign a bounded binary envelope
  containing random nonce, task ID, SessionId and monotonic expiry using a per-server
  random keyed BLAKE3 key. Validate signature/scope before lookup: invalid ID is 404,
  another session is 403, expired or evicted valid ID is 410. This avoids retaining an
  unbounded tombstone map. On every download guard the stored contract at 256 KiB, then
  recheck `Db::check`. Cache eviction drops
  only its owned TempDir; a stream keeps its own owner reference until completion. Each
  download opens a fresh file, never `try_clone` with a shared seek offset. Stream using
  `Body::from_stream(tokio_util::io::ReaderStream::new(reader))`, where reader is the
  OwnedArchiveReader retaining its TempDir owner, with a separate four-stream
  admission bound and no full-archive memory buffer.
- [ ] **Run GREEN and commit.** Run HTTP/cache, engine export and CLI export tests;
  commit `feat: download authorized task export archives`.

## Task 6: Browser verification of the review increment

**Files:** Create `tests/ui/{common,test_review}.py`, `runtime/ui-browser-requirements.txt`, `scripts/test-ui.sh`; modify `Dockerfile` and `compose.yaml` to add an opt-in browser stage/service.

**Interfaces:** `tests/ui/common.py` defines `BrowserFixture` as a context manager owning
a TemporaryDirectory and its child processes. It exposes `home`, `repo`, `contract_path`,
`launch_url`, `url`, `cli(*args) -> dict`, `seed_succeeded() -> str`, `start()`, `stop()`,
`events(task_id) -> list[dict]`, and `status(task_id) -> dict`. Fixture contract grants
snapshot.read, workspace.apply_patch, verification.run, artifact.export and model.request;
limits are 12 requests, 4096 output tokens, 50 actions, 600 seconds, one vCPU, 256 MiB.
Copy the existing parser repository, use fixtures/profiles, and preseed review tasks via
the existing CLI fake patch. `AGENTOS_UI_TEST_WORKER` accepts only host/firecracker-fake;
fake mode mirrors CLI test switches and registers the existing dummy image format.
Every child gets scrubbed provider credentials; fixtures never use a real API.

- [ ] **Write the first browser test.** Use Python unittest and the sync Playwright API:

```python
class ReviewBrowserTests(unittest.TestCase):
    def test_result_view_is_read_only_and_keyboard_reachable(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            task = fixture.seed_succeeded()
            fixture.start()
            browser = pw.chromium.launch(args=["--no-sandbox"])
            page = browser.new_page(viewport={"width": 1280, "height": 900})
            page.goto(fixture.launch_url)
            page.get_by_role("link", name="fix the parser", exact=True).click()
            before = fixture.events(task)
            page.get_by_role("tab", name="Patch", exact=True).click()
            expect(page.get_by_text("key.strip()", exact=False)).to_be_visible()
            page.get_by_role("tab", name="Verification", exact=True).click()
            expect(page.get_by_text("Verified final workspace", exact=True)).to_be_visible()
            self.assertEqual(fixture.events(task), before)
            page.set_viewport_size({"width": 390, "height": 844})
            page.keyboard.press("Tab")
            page.screenshot(path="build/ui-evidence/review-narrow.png", full_page=True)
            browser.close()
```

  Add real HTMX polling, visible 413/integrity/forbidden/stale-state panels, malicious
  text, no external asset requests and bounded event deduplication. Intercept network
  requests to fail anything outside the fixture's loopback authority. Fixture readiness
  and assertions use explicit deadlines, not fixed sleeps. Capture desktop/narrow screenshots.
- [ ] **Prepare the browser test environment and run acceptance.** Detect package
  tooling again, create a scratch venv using `pyenv exec python -m venv /tmp/agentos-ui-resolution`,
  install checked Playwright 1.63.0 using that venv's Python, freeze direct/transitive
  versions into runtime/ui-browser-requirements.txt and check their published versions.
  Build test-ui. Run `docker compose run --rm test-ui sh scripts/test-ui.sh test_review`;
  If a check fails, require an actual missing/incorrect browser behavior before fixing
  view code. If already GREEN, retain the earlier unit-test RED evidence; do not change
  correct code to manufacture a failing acceptance result.
- [ ] **Add the isolated Docker stage and script.** Name the current Dockerfile stage
  `development`; append `FROM development AS ui-test`, create `/opt/agentos-ui-venv`
  through pyenv, install its pinned requirements with its own Python and install only
  Chromium/system dependencies. Finish with `FROM development AS default` so untargeted
  builds retain the original runtime. Compose `test` targets development; `test-ui`
  extends it and targets ui-test, with init, private `shm_size: 1gb`, `network_mode: none`,
  no ports, no KVM and no extra capabilities. Browser/server share container loopback.
  `scripts/test-ui.sh` builds `agentos-cli --locked`, creates evidence output, then runs:

```sh
/opt/agentos-ui-venv/bin/python -m unittest discover -s tests/ui -p 'test_*.py' -v
```

  An optional suite argument selects `test_review.py`, `test_workflow.py` or
  `test_recovery.py`; unknown arguments fail. Use unittest directly, without pytest.
  Chromium's no-sandbox flag is confined to this disposable, offline acceptance process;
  no browser is started by the production server. Make missing browser/runtime fail,
  never silently skip the acceptance suite.
- [ ] **Run GREEN and the increment checkpoint.** Pass real browser review tests, inspect
  both screenshots, and run `sh scripts/check.sh`. Record that increment 1 delivers
  monitoring/review/export, with submission/control still absent. Commit
  `test: verify local task review in an offline browser`.

## Task 7: Shared submission and control operations

**Files:** Create `crates/agentos-cli/src/app/{submission,control}.rs`; modify command `submit.rs`, `control.rs`, `drive.rs`, application types/errors and existing CLI tests. Keep Home/model/worker configuration in its current modules.

**Interfaces:** Consume Home, TaskDetail, typed errors and existing drive/agent/executor.
Produce the following internal types and functions:

```rust
pub(crate) struct CreateRequest {
    pub contract_json: String, pub worker: WorkerKind,
    pub model: Option<String>, pub patch: Option<PathBuf>,
}
pub(crate) struct Submission { pub task_id: TaskId, pub summary: serde_json::Value }
pub(crate) struct RunRequest {
    pub task: TaskId, pub patch: Option<PathBuf>, pub crash: Option<CrashSpec>,
    pub reviewed_contract: Option<Digest>,
}
pub(crate) struct ControlOutcome {
    pub task_id: TaskId, pub state: TaskState, pub cancel_requested: bool, pub note: Option<String>,
}
// submission::create(home: &Home, request: CreateRequest) -> AppResult<Submission>
// control::prepare(home: &Home, request: RunRequest) -> AppResult<PreparedRun>
// control::drive_prepared(prepared: PreparedRun) -> async AppResult<ControlOutcome>
// control::pause(home: &Home, task: &TaskId) -> AppResult<ControlOutcome>
// control::cancel(home: &Home, task: &TaskId) -> async AppResult<ControlOutcome>
```

`PreparedRun` is an enum: `Report(ControlOutcome)`, `Drive(DrivingRun)` or
`Reconcile(DrivingRun)`. `DrivingRun` owns Home, Store, DriverLock, TaskId, optional Driver,
`RoutingExecutor<SupervisedExecutor>` and optional CrashSpec. A terminal task without
reconcilable effects returns Report without taking a driver lock or constructing a
worker/agent, preserving current CLI behavior even if KVM or a model key is unavailable.
Preparation/drive stay on the driving thread; neither store nor executor crosses a channel.
CLI adapters select their original output fields from typed outcomes; they must not
serialize new cancel/note fields onto commands that did not previously print them.

- [ ] **Write CLI compatibility and approval-boundary tests.** Add to existing `cli.rs`
  using its real helpers:

```rust
#[test]
fn staged_task_does_not_approve_until_resume() {
    let cli = Cli::new();
    let repo = cli.repo_copy();
    let contract = cli.contract(&repo);
    let created = cli.json(&["submit", &contract, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    let id = created["task_id"].as_str().unwrap();
    assert_eq!(cli.status(id)["state"], "READY");
    assert!(!cli.events(id).iter().any(|e| e["type"] == "CapabilitiesIssued"));
    assert_eq!(cli.json(&["resume", id])["state"], "SUCCEEDED");
}
```

  Freeze pause/resume/cancel JSON, pending-cancel notes, exit 1/2 errors, READY no-agent,
  terminal unresolved effects, missing key and worker mismatch. Preserve existing staged
  profile/snapshot tamper and crash-matrix tests, especially their FAILED/recovery behavior.
- [ ] **Write the application submission unit test** in `app/submission.rs`, importing
  `super::*`, `std::path::Path`, `Home`, `WorkerKind` and `TaskState`:

```rust
#[test]
fn submission_service_returns_ready_without_grants() {
    let root = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let home = Home::new(Some(root.path().join("home")), Some(fixtures.join("profiles"))).unwrap();
    let contract_json = serde_json::json!({
        "goal": "fix the parser",
        "repository": {"source": fixtures.join("parser-repo"), "revision": "recorded-at-submission"},
        "profile": "python-stdlib-v1", "verification_profile": "parser-checks-v1",
        "editable_paths": ["src/**"],
        "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
        "limits": {"model_requests": 1, "max_output_tokens_per_request": 1000,
                   "tool_actions": 10, "deadline_seconds": 600,
                   "worker_vcpus": 1, "worker_memory_mib": 256}
    }).to_string();
    let result = create(&home, CreateRequest {
        contract_json, worker: WorkerKind::Host, model: None,
        patch: Some(fixtures.join("parser-repo.fix.patch")),
    }).unwrap();
    let store = home.open().unwrap();
    assert_eq!(store.db.task(&result.task_id).unwrap().state, TaskState::Ready);
    assert!(store.db.grants(&result.task_id).unwrap().is_empty());
}
```

- [ ] **Run RED for the pure application-result unit test**, then run existing CLI tests as
  extraction guards: `docker compose run --rm test cargo test -p agentos-cli --locked`.
  New application tests call create/pause over a disposable Home and inspect typed results.
  Also assert that
  actual CLI adapters emit exactly one expected JSON response, catching duplicate service
  prints without adding a stdout-capture dependency. Runner effects are
  tested through the actual binary; unit-test executables are not supervisor binaries.
- [ ] **Extract recording, preparation and driving without changing CLI behavior.** Reuse
  submission validation, staged snapshot/profile copying and Submitted audit verbatim.
  `create` returns READY and the original summary. CLI submit --yes acquires its driver
  lock before recording, as today; share a private `create_locked` helper so extraction
  does not introduce a new READY task on busy CLI submit. `prepare` owns lock and exact
  recorded worker/model/endpoint/policy, performs current preflight, then approval/resume.
  Terminal in-flight effects take Reconcile mode without constructing a new agent.
- [ ] **Add explicit reviewed-input validation without altering CLI tamper behavior.**
  Extract `drive::input_problem(home, store, task) -> Result<Option<String>, CliError>`
  as a pure helper; existing `check_inputs` still converts a problem to Failed and recovers
  outstanding effects. For UI READY requests with `reviewed_contract=Some(digest)`,
  `prepare` compares the stored contract digest and input_problem after acquiring the
  lock but before issuing capabilities. Reject a mismatch as Conflict while still READY.
  CLI uses None and retains the old failure/recovery route. Non-READY UI resume cannot
  implicitly approve an unrelated READY task.
- [ ] **Run GREEN and commit.** Run CLI unit/integration suites and crash/recovery
  regressions in host/fake-jail modes; commit `refactor: share task submission and control operations`.

## Task 8: Runner admission and web controls

**Files:** Create `crates/agentos-cli/src/app/runner.rs`; extend UI state/routes/error handling, action templates and HTTP integration tests.

**Interfaces:** Consume RunRequest, PreparedRun and ControlOutcome. Produce
`RunnerManager::new(home: Home)`,
`RunnerManager::admit(request: RunRequest) -> async AppResult<RunAdmission>`, and
`RunnerManager::drain() -> async AppResult<()>`. Owned manager state tracks one admitted
thread, its TaskId and completion. It uses a gate to reject concurrent admissions and
the real Home driver lock to reject other processes; it is not a queue. Completion
status is journal-derived; an in-memory runner error is a diagnostic, never a new task state.
`RunAdmission` is `Started(TaskId)` or `Reported(ControlOutcome)`: only Started maps to
202; a no-work Report returns a 200 status fragment. Manager lifecycle also tracks at
most four owned auxiliary control operations with admission rejection rather than an
unbounded request-thread queue; shutdown drain waits for them as well as the driver.

- [ ] **Write runner/concurrency and actual HTTP action tests.** Use a test-only admission
  barrier to hold a prepared worker before completion; disconnect its request and verify
  the thread continues. Real HTTP assertions:

```rust
let response = session.post(&server, &format!("/tasks/{id}/start"),
    &[("contract_digest", reviewed_digest.as_str())]).await;
assert_eq!(response.status(), 202);
let duplicate = session.post(&server, &format!("/tasks/{id}/start"),
    &[("contract_digest", reviewed_digest.as_str())]).await;
assert_eq!(duplicate.status(), 409);
assert_eq!(fixture.capabilities_issued_count(&id), 1);
```

  Extend UiSession with `post(server, path, fields) -> async reqwest::Response`, applying
  exact Origin/CSRF; UiFixture adds bounded helpers for event counts and a slow verification
  profile. Test an external CLI lock holder, pause while an effect is active, cancellation
  pending under another driver, terminal reconciliation, server shutdown drain, admission
  preflight error and panic-safe release of the in-process gate. Verify normal queries
  still respond while the runner waits for verification.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-cli --test ui --locked web_controls`, plus runner unit tests.
- [ ] **Implement the owned driving thread and admission acknowledgement.** Thread
  constructs its own current-thread Tokio runtime, Store, agent/executor and lock. It
  runs Task 7 prepare, reports typed admission via tokio oneshot, then awaits drive on
  its own runtime. No Db/PreparedRun crosses the channel. Request cancellation does not
  cancel preparation/drive. If admission fails, release the gate and return the existing
  reason; if admitted, return 202 with current durable status and continue independently.
  Panic/exit releases the lock and exposes recovery instead of fabricating cancellation.
- [ ] **Wire state-correct action forms and shutdown.** Start requires READY plus reviewed
  digest; resume rejects READY so its route cannot bypass review. Pause uses existing
  state/reducer rules. Cancel invokes the shared operation on an owned operation thread
  when reconciliation can wait, leaving the HTTP request responsive and reporting pending
  cancellation from DB. Ensure these operations also survive request disconnect; they
  use the same real driver lock, not the manager's start gate. Reconcile terminal effects
  through prepare's explicit mode only when the existing resume path would do work
  (INTENDED/DISPATCHED); UNKNOWN-only terminal tasks retain their uncertainty and must
  not claim a completed reconciliation. On normal shutdown stop admission and await
  admitted operations; forced termination preserves recovery. No automatic start on
  server boot.
- [ ] **Run GREEN and commit.** Run controls/concurrency, CLI parity and session tests;
  commit `feat: control journaled task runs from the local dashboard`.

## Task 9: Contract input, duplicate admission and approval review

**Files:** Create `crates/agentos-cli/src/ui/forms.rs`, templates `ui/{new,approval,created}.html`; extend app queries/submission, local JS, routes and HTTP tests.

**Interfaces:** Consume TaskDetail, CreateRequest, Submission and RunnerManager. Produce
`FormNonce(String)` and `SubmissionForms::issue(session) -> AppResult<FormNonce>`;
`submit(session, nonce, request) -> async AppResult<Submission>` coalesces issued/pending/
completed entries. Issuance/expiry use the same monotonic-clock approach as downloads;
unknown/restarted/expired nonces fail, and a pending operation is never evicted.

- [ ] **Write form/approval regressions.** Actual browser-shaped HTTP test:

```rust
let first = session.create(&server, &nonce, &contract_json, &model_spec).await;
let repeated = session.create(&server, &nonce, &contract_json, &model_spec).await;
assert_eq!(first.task_id, repeated.task_id);
let before = fixture.events(&first.task_id);
assert!(!before.iter().any(|e| e["type"] == "CapabilitiesIssued"));
fixture.change_staged_profile(&first.task_id);
let denied = session.post(&server, &format!("/tasks/{}/start", first.task_id),
    &[("contract_digest", first.contract_digest.as_str())]).await;
assert_eq!(denied.status(), 409);
assert_eq!(fixture.status(&first.task_id)["state"], "READY");
assert_eq!(fixture.events(&first.task_id), before);
```

  UiSession::create parses the successful created page's `data-task-id` and
  `data-contract-digest`; return `CreatedTask { task_id: String, contract_digest: String }`.
  Define fixture change_staged_profile only against its owned home. Add lost response,
  concurrent same nonce, reused nonce with a different body, invalid JSON/schema/model,
  missing repository/profile/image/key, fake transcript, original source changing after
  snapshot recording, forged reviewed digest, nonce expiry/full-pending-map and stale
  session after restart. Different bodies for a pending/completed nonce conflict rather
  than silently changing its task. Client button state is never the duplicate guard.
- [ ] **Run RED:** `docker compose run --rm test cargo test -p agentos-cli --test ui --locked contract_form`, plus form admission unit tests.
- [ ] **Implement JSON form input and immutable review.** Use a local file input that
  reads JSON into the textarea, without upload filesystem paths. Accept model spec and
  host/firecracker selection, never credentials/endpoints/binary paths. Web forms require
  a model spec for runnable tasks; existing CLI no-agent READY behavior remains valid,
  shown as unavailable to start until a CLI operator supplies an agent. Pass contract
  bytes directly into shared create rather than composing subprocess argv. Profile/image
  choices are read-only registries; display actual server filesystem paths explicitly.
- [ ] **Implement server-owned nonce admission and confirmation.** Register nonces when
  issuing a form. Store a body digest with pending/result state so duplicate bodies wait
  for and return the same task; a different body conflicts. Keep pending work alive on
  an owned thread, not a request-owned future; publish completion even if the receiver
  drops. Evict only completed/unused expired forms and reject full pending admission.
  Redirect success to the recorded READY review page showing all permissions/limits/
  provenance/digests and an explicit Approve and start form. Buttons/errors use HTMX with
  ordinary form fallback and accessible field labels. Polling never posts approval.
- [ ] **Run GREEN and commit.** Run form/start HTTP tests, nonce unit tests and CLI parity;
  commit `feat: create and approve recorded task contracts in the browser`.

## Task 10: Full workflow, recovery, Compose launch and CI

**Files:** Create `tests/ui/{test_workflow,test_recovery}.py`, `docs/ui.md`, `.github/workflows/ui.yml`, `docs/reviews/2026-10-04-local-web-ui-review.md`; extend browser fixtures/script, Compose, README and evidence docs. Update spec/plan progress only to reflect actual evidence.

**Interfaces:** Extend BrowserFixture with `create_contract_json() -> str`,
`wait_state(task_id, state, timeout=60)`, `slow_profile()` and `restart()` over the same home.
The CLI remains the state/journal oracle. Capture Chrome failures and screenshots into
`build/ui-evidence/`; never collect launch credentials/provider keys in uploaded artifacts.

- [ ] **Write the full browser acceptance.** Use the exact accessible labels introduced
  by Tasks 4/9, with recorded contract JSON and `fake:/work/fixtures/transcripts/parser-fix.json`:

```python
page.get_by_role("link", name="New task", exact=True).click()
page.get_by_label("Task contract JSON").fill(fixture.create_contract_json())
page.get_by_label("Model").fill("fake:/work/fixtures/transcripts/parser-fix.json")
page.get_by_role("button", name="Create task", exact=True).click()
task = page.locator("[data-task-id]").first.get_attribute("data-task-id")
self.assertEqual(fixture.status(task)["state"], "READY")
self.assertFalse(any(e["type"] == "CapabilitiesIssued" for e in fixture.events(task)))
page.get_by_role("button", name="Approve and start", exact=True).click()
fixture.wait_state(task, "SUCCEEDED")
page.get_by_role("tab", name="Patch", exact=True).click()
expect(page.get_by_text("key.strip()", exact=False)).to_be_visible()
page.get_by_role("tab", name="Verification", exact=True).click()
expect(page.get_by_text("Verified final workspace", exact=True)).to_be_visible()
with page.expect_download() as download:
    page.get_by_role("button", name="Export", exact=True).click()
download.value.save_as(fixture.home.parent / "result.tar")
```

  Extract the generated archive into a disposable test directory with Python tarfile's
  data filter, compare paths and patch/evidence bytes with a CLI export, and assert the
  original source repository digest is unchanged. Download-link creation must initiate
  the browser attachment through local JS or a visible fallback link, not silently save
  to an arbitrary server location.
- [ ] **Write lifecycle/browser-failure tests.** With a slow protected verification
  fixture, wait for a dispatched verification event, close the tab and confirm progress.
  In another case SIGKILL only the fixture UI controller, record completed effect IDs,
  restart/authenticate and verify no auto-resume; click Recover and compare those effects
  for single completion and unchanged identity. Also exercise CLI/browser pause and cancel,
  restart with an old nonce/session, outdated action forms, missing key/preflight refusal,
  revoked cached exports, stale-state errors and keyboard/narrow layouts. Use bounded
  polling/barriers rather than timing guesses; allow the existing supervisor to reconcile.
- [ ] **Run browser RED/GREEN verification:** run Compose browser workflow/recovery suites against the assembled
  implementation before any last fixes. Failure output must identify the violated UI or
  lifecycle behavior; retain red/green logs and exclude all secrets from evidence.
- [ ] **Finish local Compose launch and docs.** Add `ui` extending the development test
  service with `network_mode: host`, no ports/extra caps, source mount read-only (target
  cache remains writable), and a new `ui-home` volume mounted at `/data`. Command:

```yaml
command: ["cargo", "run", "--locked", "-p", "agentos-cli", "--", "--home", "/data", "--profiles", "/work/fixtures/profiles", "ui", "--port", "8080"]
```

  Document Linux-only host networking, server-visible repository paths, persistent-home
  selection, CLI registry setup, local launch-link access, two-step approval, one driver,
  browser/server shutdown differences, explicit recovery, export size/expiry and fixture-
  versus real acceptance. No key is built into images or supplied by default. Existing
  homes are selected by explicit mount; do not overwrite or clean them.
- [ ] **Add the UI CI job and run final gates.** On supported Ubuntu runner, build test-ui,
  run host browser acceptance and fake-worker browser acceptance with the CLI's existing
  fake jail switches, then upload only sanitized screenshots/test logs on failure. Keep
  the existing Rust CI workflow and its four gates. Exact local checks:

```sh
sh scripts/check.sh
docker compose build test-ui
docker compose run --rm test-ui sh scripts/test-ui.sh
docker compose run --rm -e AGENTOS_UI_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test-ui sh scripts/test-ui.sh
git diff --check
```

  Inspect desktop/narrow screenshots from actual rendered fixture pages. Browser tests
  must fail for a missing runtime and use no provider credentials, external network or KVM.
- [ ] **Review and commit the completed milestone.** Follow the selected execution
  method's independent-review policy; do not claim an unreviewed branch was approved.
  Fix validated defects with failing regressions, rerun affected checks and final required
  gates after code changes. Record commit/evidence tiers and limitations in the review
  document. Commit `feat: complete local browser task run and review workflow` without
  merging/pushing unless separately authorized.

## Coverage and handoff

| Spec requirement | Owning tasks |
| --- | --- |
| CLI parity and shared operations | 2, 5, 7 |
| Paged task list and bounded reads | 1, 2, 3 |
| Pure result review and exact verification claims | 2, 4 |
| Local session, Origin/Host/CSRF, escaping, errors | 3, 4, 9 |
| Export authority, bytes, streaming, expiry/cleanup | 5, 10 |
| Actual browser layout/keyboard/polling evidence | 6, 10 |
| Contract recording, explicit approval and duplicate handling | 7, 9 |
| One driver, disconnect lifetime, pause/cancel/recovery | 7, 8, 10 |
| Compose runtime, Python isolation and required CI checks | 6, 10 |
| Real-provider/KVM exclusions and operational docs | 10 |

Inline self-review checked the spec against this table, interface producers/consumers,
all five Review Focus conditions against their test steps, placeholders and source links.
It resolved bounded-contract guards before legacy queries/authorization, journaled patch
budgeting, terminal report/reconcile admission, archive stream ownership and CLI tamper
compatibility. Six planning-artifact checks passed through Docker Compose using pyenv:
references, code fences/placeholders, task structure, coverage/review conditions, recorded
bases/approval status, and exact limits/checkpoints. Planning validation is not
implementation acceptance; no UI product code or dependency changes are in this commit.

Recommend native execution: these ten tasks depend closely on the same store/application
interfaces, and native work keeps their extraction consistent with one independent
whole-branch review at the end. Subagent-driven execution remains an explicit alternative
with per-task implementation/review gates. The owner reviews this saved plan and selects
the execution method before product code, dependency installation or scaffolding begins.
