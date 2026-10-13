# Read-only cockpit — design

Status: draft for review, 2026-10-13. Next roadmap item after workflows as chained contracts
(`main` tip a627d2b). Later item, out of scope here: fan-out.

## Purpose

An overview of everything in one agent-os home directory. `agentos cockpit` opens a full-screen
terminal UI that lists every workflow and task with its state and cost, and lets the owner open
one to see its steps or its events. It never changes state.

## Decisions (agreed 2026-10-13)

1. **Purpose:** overview of everything (a list of all tasks and workflows, with drill-in).
2. **Form:** terminal UI (ratatui), no server, no ports; works over SSH.
3. **Drill-in:** summary plus events. Not the effect journal, grants, diffs or verification output.
4. **Architecture:** a read-only store view plus a separate TUI crate (`agentos-cockpit`).

Rejected: the TUI inside `agentos-cli` (pulls a terminal framework into the CLI crate and is
harder to test alone), and reusing the normal `Store` handle while promising not to write
(read-only would be a convention, and the normal open migrates and can lock).

## Non-goals (v1)

Actions or approvals (pause, cancel, resume), a network or web surface, the effect journal,
capability grants, patch diffs, verification output, search, filters, sorting options, a
multi-home view, and notifications.

## 1. Read path (store)

- `View::open(path)` opens the same SQLite file with `SQLITE_OPEN_READ_ONLY` and
  `PRAGMA query_only=ON`. `View` is a separate type from `Db`, because `Db` has write methods and
  the cockpit must not be able to call them. It takes no home lock and runs no migration and no
  `journal_mode` change. It never creates the home or the database file (a missing file is
  `ViewError::Missing`); opening a WAL database read-only may create empty `-wal`/`-shm` side
  files next to it when no controller is running (SQLite behaviour).
- It refuses a database whose `user_version` is not the current `SCHEMA_VERSION`, with a message
  that names the fix (run any other agentos command to upgrade). It never migrates.
- `View` sets a short `busy_timeout`. WAL lets it read while a controller writes. A busy read is the
  caller's problem to retry (see "Refresh").
- New queries on `View`, each returning a small row struct, each limited (default 500, newest
  first):
  - `task_rows()`: id, state, goal, limits, usage counts and created time of tasks outside any
    workflow (a step task is read through its workflow, so step tasks never crowd loose tasks
    out of the window).
  - `workflow_rows()`: id, name, step names with task ids and states, and the cancel flag. All
    steps of the windowed workflows come from one query, and usage is summed once per snapshot.
  - The snapshot keeps the newest `limit` of the two lists together and says when it cut
    anything. A row whose stored values cannot be parsed is skipped and counted, so one corrupt
    row does not blank the list.
  - `event_rows(task, after_seq, limit)`: seq, type and ts only (no payload).
  - `event_payload(task, seq)`: one event's stored payload.
- A workflow's state is the existing `derive_state` over its steps' task states. The cockpit has
  no second implementation of state.
- Test: open read-only, assert every write attempt fails, and assert that a write made on another
  connection after the open is visible to the reader.

## 2. TUI (`agentos-cockpit`)

- The crate depends on `agentos-store` and `agentos-core` for types, and on `ratatui` 0.30.2
  (current at 2026-10-13). It does not depend on `agentos-engine` or any controller code.
- `agentos cockpit` in the CLI is a thin wrapper: resolve the home directory, open the read-only
  view, call `agentos_cockpit::run`. If stdout is not a terminal it exits 2 with a clear message.
- **List screen:** one table of workflows and tasks, newest first. Columns: kind, id, name or
  goal, state, step progress (workflow only), used versus limit for model requests and tool
  actions, and age. A task that belongs to a workflow step appears under its workflow, not as a
  separate top-level row.
- **Detail screen:** a workflow shows its steps with task state and budget. A task shows its
  contract terms and a scrolling list of events (seq, type, time).
- **Keys:** up/down and j/k move, Enter opens, Esc or q goes back or quits, Ctrl-C quits, `p` shows the
  raw payload of the selected event, `r` forces a refresh. On a payload, up/down, j/k and
  PgUp/PgDn scroll it. No key performs an action. The list keeps its selection by item, not by
  row, so items arriving above it do not move it; only the answer to the payload request last
  made from the open task opens a payload.
- **Refresh:** a background thread polls about once a second and sends frames over a channel;
  contract frames carry the task id they belong to, and frames for a task that is not open are
  ignored, so
  the UI never blocks on SQLite. A failed or busy read keeps the last frame and marks the header
  "stale".
- **Safe rendering:** every string from the store has control characters and escape sequences
  stripped before it is drawn, so a hostile goal or event type cannot move the cursor or inject
  terminal escapes. Long lines are truncated.
- **Payloads:** `agentos events` prints payloads as stored, with no redaction. The cockpit
  therefore shows no payload in the list or by default. `p` shows one event's payload once, capped at
  65,536 chars and sanitized as above (control and format characters, and U+2028/U+2029, are
  dropped). A JSON payload is re-indented one value per line, keeping its key order and digits;
  a payload cut by the cap, or not JSON, is shown raw. The README says payloads are shown as stored and may contain task
  content.

## 3. Errors and limits

- Missing or unreadable home: exit 2 with the path and the reason. A missing home is never
  created. The database is opened before the terminal check, so these refusals do not need a
  terminal.
- Wrong schema version: refuse (section 1).
- Empty home: show "no tasks or workflows yet" with the `submit` and `workflow submit` hints.
  Before the first read the list says "loading…"; if that read failed it shows the reason.
- The terminal is restored on any exit, including a panic. Ctrl-C quits, as do `q` and Esc on the list. A resize redraws.
- Large homes: the list shows the newest 500 items (workflows and loose tasks together) and the
  header says "showing newest N" when more exist, and how many unreadable rows were skipped. A task's events
  show the latest 200; when the selection reaches the top, an older page of 200 loads, and new
  events are appended on refresh.
- Ages use the host clock and are display only.

## Testing

All through `docker compose run --rm test ...`.

- Store: read-only open refuses writes, refuses another schema version, sees later writes from
  another connection, and each list query respects its limit and order.
- Rendering: ratatui `TestBackend` snapshots over a fixture home for the list, workflow detail,
  task detail, an empty home, and a stale frame.
- Sanitizer: unit tests feed escape sequences, C0/C1 controls and very long lines through it.
- CLI: `agentos cockpit` exits 2 when stdout is not a terminal, and when the home is missing.
- Key handling: a test sends every key and asserts none opens a write path (the cockpit holds
  only the read-only handle, so this is also checked by the type it is given).
- No live run: the cockpit only reads local state.

## Documentation

A short "Cockpit" section in the README: how to start it, the keys, what is shown, and the
payload note.
