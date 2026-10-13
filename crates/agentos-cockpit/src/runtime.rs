//! The cockpit's two threads: a worker that is the only reader of the store, and the terminal
//! loop that turns keys into requests and frames into screens.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::ids::TaskId;
use agentos_store::view::{EventPage, View, ViewError};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::{App, Command, EventUpdate, Frame};
use crate::render::draw;

/// The newest tasks and workflows each frame carries.
const SNAPSHOT_LIMIT: usize = 500;
/// Events per page, both the first page of a task and each older one.
const PAGE: usize = 200;
const PAYLOAD_CHARS: usize = 65_536;
/// How often the worker reads the store when nothing is asked of it.
const POLL: Duration = Duration::from_secs(1);
/// How long the terminal loop waits for a key before it redraws.
const TICK: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    View(#[from] ViewError),
    #[error("terminal: {0}")]
    Io(#[from] std::io::Error),
}

/// What the terminal loop asks of the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Refresh,
    Watch(Option<TaskId>),
    Older(TaskId, u64),
    Payload(TaskId, u64),
}

/// The task whose events the worker follows.
struct Watched {
    task: TaskId,
    /// The newest seq delivered; `None` until the first page went out.
    last: Option<u64>,
    contract_sent: bool,
}

/// The worker loop: the only user of `view`. Ends when either channel closes.
pub fn serve(view: View, requests: Receiver<Request>, frames: Sender<Frame>, poll: Duration) {
    let mut watched: Option<Watched> = None;
    loop {
        let frame = match requests.recv_timeout(poll) {
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) | Ok(Request::Refresh) => tick(&view, watched.as_mut()),
            Ok(Request::Watch(task)) => {
                watched = task.map(|task| Watched {
                    task,
                    last: None,
                    contract_sent: false,
                });
                tick(&view, watched.as_mut())
            }
            Ok(Request::Older(task, before)) => older(&view, task, before),
            Ok(Request::Payload(task, seq)) => payload(&view, task, seq),
        };
        if frames.send(frame).is_err() {
            return;
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A frame with a fresh snapshot and nothing else.
fn base(view: &View) -> Frame {
    Frame {
        snapshot: view.snapshot(SNAPSHOT_LIMIT).map_err(|e| e.to_string()),
        now: now(),
        events: None,
        contract: None,
        payload: None,
    }
}

/// Marks the frame stale with `e` unless an earlier read already did. A failed event or
/// contract read makes the frame stale, while `apply_frame` still applies the rows it carries.
fn fail(frame: &mut Frame, e: ViewError) {
    if frame.snapshot.is_ok() {
        frame.snapshot = Err(e.to_string());
    }
}

fn tick(view: &View, watched: Option<&mut Watched>) -> Frame {
    let mut frame = base(view);
    let Some(w) = watched else {
        return frame;
    };
    match follow(view, w) {
        Ok(events) => frame.events = events,
        Err(e) => fail(&mut frame, e),
    }
    if !w.contract_sent {
        match view.contract(&w.task) {
            Ok(contract) => {
                w.contract_sent = true;
                frame.contract = contract.map(|c| (w.task.clone(), c));
            }
            Err(e) => fail(&mut frame, e),
        }
    }
    frame
}

/// The latest page the first time, then whatever came after the last delivered seq.
fn follow(view: &View, w: &mut Watched) -> Result<Option<EventUpdate>, ViewError> {
    let page = w.last.map_or(EventPage::Latest, EventPage::After);
    let rows = view.events(&w.task, page, PAGE)?;
    if w.last.is_some() && rows.is_empty() {
        return Ok(None);
    }
    let total = view.event_count(&w.task)?;
    let task = w.task.clone();
    let first = w.last.is_none();
    w.last = Some(rows.last().map_or(w.last.unwrap_or(0), |r| r.seq));
    Ok(Some(if first {
        EventUpdate::Replace { task, rows, total }
    } else {
        EventUpdate::Append { task, rows, total }
    }))
}

fn older(view: &View, task: TaskId, before: u64) -> Frame {
    let mut frame = base(view);
    let read = view
        .events(&task, EventPage::Before(before), PAGE)
        .and_then(|rows| Ok((rows, view.event_count(&task)?)));
    match read {
        Ok((rows, _)) if rows.is_empty() => {}
        Ok((rows, total)) => frame.events = Some(EventUpdate::Prepend { task, rows, total }),
        Err(e) => fail(&mut frame, e),
    }
    frame
}

fn payload(view: &View, task: TaskId, seq: u64) -> Frame {
    let mut frame = base(view);
    match view.payload(&task, seq, PAYLOAD_CHARS) {
        Ok(text) => frame.payload = Some((task, seq, text)),
        Err(e) => fail(&mut frame, e),
    }
    frame
}

/// The worker request for a command; `Quit` and `None` ask nothing of it.
pub fn to_request(command: &Command) -> Option<Request> {
    match command {
        Command::None | Command::Quit => None,
        Command::Refresh => Some(Request::Refresh),
        Command::Watch(task) => Some(Request::Watch(task.clone())),
        Command::Older(task, before) => Some(Request::Older(task.clone(), *before)),
        Command::Payload(task, seq) => Some(Request::Payload(task.clone(), *seq)),
    }
}

/// What the terminal loop does after one terminal event.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Quit,
    Send(Request),
    Redraw,
}

fn on_event(app: &mut App, event: &Event) -> Step {
    match event {
        // Raw mode turns Ctrl-C into a key, so it quits here from every screen.
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Step::Quit
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => match app.handle_key(key.code) {
            Command::Quit => Step::Quit,
            command => to_request(&command).map_or(Step::Redraw, Step::Send),
        },
        _ => Step::Redraw,
    }
}

/// Applies every waiting frame; `false` once the worker is gone.
fn drain(app: &mut App, frames: &Receiver<Frame>) -> bool {
    loop {
        match frames.try_recv() {
            Ok(frame) => app.apply_frame(frame),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// Opens the store before touching the terminal, so a missing database or another schema
/// is reported on a normal screen. Nothing here creates a file.
pub fn run(db_path: &Path) -> Result<(), RunError> {
    let view = View::open(db_path)?;
    let (requests, worker_requests) = mpsc::channel();
    let (worker_frames, frames) = mpsc::channel();
    let worker = thread::spawn(move || serve(view, worker_requests, worker_frames, POLL));
    // The first frame now rather than after a poll.
    let _ = requests.send(Request::Refresh);
    // `try_init` also installs a panic hook that restores the terminal. Without a terminal it
    // fails, possibly after entering raw mode, so restore runs on that path too.
    let result = ratatui::try_init().and_then(|mut terminal| ui(&mut terminal, &requests, &frames));
    ratatui::restore();
    drop(requests);
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
    Ok(result?)
}

fn ui(
    terminal: &mut DefaultTerminal,
    requests: &Sender<Request>,
    frames: &Receiver<Frame>,
) -> std::io::Result<()> {
    let mut app = App::new();
    loop {
        terminal.draw(|f| draw(f, &app))?;
        let ready = event::poll(TICK)?;
        // Before the next draw, so a worker gone mid-wait ends the loop without one.
        if !drain(&mut app, frames) {
            return Ok(());
        }
        if !ready {
            continue;
        }
        match on_event(&mut app, &event::read()?) {
            Step::Quit => return Ok(()),
            Step::Send(request) => {
                if requests.send(request).is_err() {
                    return Ok(());
                }
            }
            Step::Redraw => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{EventUpdate, Screen};
    use agentos_store::db::Db;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    const CONTRACT_JSON: &str = r#"{
    "goal": "fix the parser",
    "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
    "profile": "protected", "editable_paths": ["src/"],
    "verification_profile": "parser-checks-v1",
    "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run"],
    "limits": {"model_requests": 10, "max_output_tokens_per_request": 1000, "tool_actions": 1000,
               "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256}
}"#;

    fn start(
        path: &std::path::Path,
    ) -> (
        Sender<Request>,
        Receiver<Frame>,
        std::thread::JoinHandle<()>,
    ) {
        let view = View::open(path).unwrap();
        let (rtx, rrx) = mpsc::channel();
        let (ftx, frx) = mpsc::channel();
        let handle = std::thread::spawn(move || serve(view, rrx, ftx, Duration::from_millis(20)));
        (rtx, frx, handle)
    }

    fn next(frames: &Receiver<Frame>) -> Frame {
        frames
            .recv_timeout(Duration::from_secs(5))
            .expect("a frame in time")
    }

    fn frame_with_events(frames: &Receiver<Frame>) -> Frame {
        loop {
            let f = next(frames);
            if f.events.is_some() {
                return f;
            }
        }
    }

    fn frame_with_payload(frames: &Receiver<Frame>) -> Frame {
        loop {
            let f = next(frames);
            if f.payload.is_some() {
                return f;
            }
        }
    }

    fn task_with_notes(dir: &tempfile::TempDir, notes: usize) -> (Db, std::path::PathBuf, TaskId) {
        let path = dir.path().join("agentos.db");
        let db = Db::open(&path).unwrap();
        let c = agentos_core::contract::Contract::parse(CONTRACT_JSON).unwrap();
        let id = db
            .create_task(&c, &agentos_core::ids::Digest::of(CONTRACT_JSON.as_bytes()))
            .unwrap();
        for i in 0..notes {
            db.append_audit(&id, "Note", &serde_json::json!({ "i": i, "pad": "x" }))
                .unwrap();
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
        assert!(first.snapshot.is_ok());
        let (contract_task, contract) = first.contract.expect("the contract with the first frame");
        assert_eq!(
            (contract_task, contract.goal.as_str()),
            (id.clone(), "fix the parser")
        );
        let Some(EventUpdate::Replace { rows, total, .. }) = first.events else {
            panic!("Replace")
        };
        assert_eq!((rows.len() as u64, total), (6, 6)); // TaskCreated + 5 notes
        db.append_audit(&id, "Note", &serde_json::json!({ "late": true }))
            .unwrap();
        let next = frame_with_events(&frames);
        assert!(
            next.contract.is_none(),
            "the contract is sent once per Watch"
        );
        let Some(EventUpdate::Append { rows, total, .. }) = next.events else {
            panic!("Append")
        };
        assert_eq!(
            (rows.len(), total, rows[0].event_type.as_str()),
            (1, 7, "Note")
        );
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn older_pages_prepend_and_a_payload_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, id) = task_with_notes(&dir, 250); // 251 events with TaskCreated
        let (requests, frames, worker) = start(&path);
        requests.send(Request::Watch(Some(id.clone()))).unwrap();
        let Some(EventUpdate::Replace { rows, total, .. }) = frame_with_events(&frames).events
        else {
            panic!("Replace")
        };
        assert_eq!((rows.len(), total, rows[0].seq), (200, 251, 52));
        requests
            .send(Request::Older(id.clone(), rows[0].seq))
            .unwrap();
        let Some(EventUpdate::Prepend { rows, .. }) = frame_with_events(&frames).events else {
            panic!("Prepend")
        };
        assert_eq!(
            (
                rows.len(),
                rows.first().map(|r| r.seq),
                rows.last().map(|r| r.seq)
            ),
            (51, Some(1), Some(51))
        );
        requests.send(Request::Payload(id.clone(), 251)).unwrap();
        let payload = frame_with_payload(&frames).payload.unwrap();
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

    #[test]
    fn a_quiet_refresh_carries_no_events() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, id) = task_with_notes(&dir, 2);
        let (requests, frames, worker) = start(&path);
        requests.send(Request::Watch(Some(id))).unwrap();
        frame_with_events(&frames);
        for _ in 0..5 {
            requests.send(Request::Refresh).unwrap();
        }
        for _ in 0..10 {
            let f = next(&frames);
            assert!(f.snapshot.is_ok() && f.events.is_none() && f.contract.is_none());
        }
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn watching_a_task_without_events_still_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, _id) = task_with_notes(&dir, 0);
        let (requests, frames, worker) = start(&path);
        let absent = TaskId::new();
        requests.send(Request::Watch(Some(absent.clone()))).unwrap();
        let f = frame_with_events(&frames);
        let Some(EventUpdate::Replace { task, rows, total }) = f.events else {
            panic!("Replace")
        };
        assert_eq!((task, rows.len(), total), (absent, 0, 0));
        assert!(f.contract.is_none(), "no such task, no contract");
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn watching_the_same_task_again_replaces_and_resends_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, id) = task_with_notes(&dir, 1);
        let (requests, frames, worker) = start(&path);
        for _ in 0..2 {
            requests.send(Request::Watch(Some(id.clone()))).unwrap();
            let f = frame_with_events(&frames);
            assert!(matches!(f.events, Some(EventUpdate::Replace { .. })));
            assert_eq!(f.contract.map(|(t, _)| t), Some(id.clone()));
        }
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn after_watch_none_no_events_or_contract_are_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, id) = task_with_notes(&dir, 1);
        let (requests, frames, worker) = start(&path);
        requests.send(Request::Watch(Some(id.clone()))).unwrap();
        frame_with_events(&frames);
        requests.send(Request::Watch(None)).unwrap();
        // A marker: every frame after its answer was made after Watch(None).
        requests.send(Request::Payload(id.clone(), 1)).unwrap();
        frame_with_payload(&frames);
        db.append_audit(&id, "Note", &serde_json::json!({ "late": true }))
            .unwrap();
        for _ in 0..10 {
            let f = next(&frames);
            assert!(f.events.is_none() && f.contract.is_none());
        }
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn a_failed_snapshot_is_sent_as_an_error_and_the_worker_keeps_going() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, path, id) = task_with_notes(&dir, 0);
        let (requests, frames, worker) = start(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        let set = |json: &str| {
            conn.execute(
                "UPDATE tasks SET contract_json = ?1 WHERE id = ?2",
                [json, id.as_str()],
            )
            .unwrap()
        };
        set("not json");
        let reason = loop {
            if let Err(reason) = next(&frames).snapshot {
                break reason;
            }
        };
        assert!(reason.contains("corrupt"), "{reason}");
        set(CONTRACT_JSON);
        loop {
            if next(&frames).snapshot.is_ok() {
                break;
            }
        }
        drop(requests);
        worker.join().unwrap();
    }

    #[test]
    fn a_missing_database_is_reported_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentos.db");
        assert!(matches!(
            run(&path),
            Err(RunError::View(ViewError::Missing(p))) if p == path
        ));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn commands_map_to_requests_and_quit_and_none_to_nothing() {
        let id = TaskId::new();
        assert_eq!(to_request(&Command::Quit), None);
        assert_eq!(to_request(&Command::None), None);
        assert_eq!(to_request(&Command::Refresh), Some(Request::Refresh));
        assert_eq!(
            to_request(&Command::Watch(None)),
            Some(Request::Watch(None))
        );
        assert_eq!(
            to_request(&Command::Watch(Some(id.clone()))),
            Some(Request::Watch(Some(id.clone())))
        );
        assert_eq!(
            to_request(&Command::Older(id.clone(), 7)),
            Some(Request::Older(id.clone(), 7))
        );
        assert_eq!(
            to_request(&Command::Payload(id.clone(), 9)),
            Some(Request::Payload(id, 9))
        );
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind))
    }

    #[test]
    fn only_key_presses_count_and_a_resize_just_redraws() {
        let mut app = App::new();
        assert_eq!(on_event(&mut app, &Event::Resize(80, 24)), Step::Redraw);
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert_eq!(
                on_event(&mut app, &key(KeyCode::Char('q'), kind)),
                Step::Redraw
            );
        }
        assert_eq!(
            on_event(&mut app, &key(KeyCode::Char('r'), KeyEventKind::Press)),
            Step::Send(Request::Refresh)
        );
        assert_eq!(
            on_event(&mut app, &key(KeyCode::Char('z'), KeyEventKind::Press)),
            Step::Redraw
        );
        assert_eq!(
            on_event(&mut app, &key(KeyCode::Char('q'), KeyEventKind::Press)),
            Step::Quit
        );
    }

    fn ctrl_c(kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            kind,
        ))
    }

    #[test]
    fn ctrl_c_quits_from_every_screen_and_a_plain_c_or_a_release_does_not() {
        let (on_task, id) = crate::app::fixtures::app_on_task(1..=3);
        let mut on_payload = on_task.clone();
        on_payload.apply_frame(Frame {
            payload: Some((id, 3, Some(("x".into(), false)))),
            ..crate::app::fixtures::good(on_payload.snapshot.clone().unwrap())
        });
        assert!(matches!(on_payload.screen, Screen::Payload { .. }));
        for app in [App::new(), on_task, on_payload] {
            let screen = app.screen.clone();
            let mut a = app.clone();
            assert_eq!(
                on_event(&mut a, &ctrl_c(KeyEventKind::Press)),
                Step::Quit,
                "{screen:?}"
            );
            let mut a = app.clone();
            assert_eq!(
                on_event(&mut a, &ctrl_c(KeyEventKind::Release)),
                Step::Redraw,
                "{screen:?}"
            );
            let mut a = app.clone();
            assert_ne!(
                on_event(&mut a, &key(KeyCode::Char('c'), KeyEventKind::Press)),
                Step::Quit,
                "{screen:?}"
            );
            assert_eq!(a.screen, screen, "a plain c changes nothing");
        }
    }

    #[test]
    fn opening_a_task_sends_watch_and_leaving_it_sends_watch_none() {
        let t = crate::app::fixtures::task("g", 1, None);
        let id = t.id.clone();
        let mut app = App::new();
        app.apply_frame(crate::app::fixtures::good(agentos_store::view::Snapshot {
            tasks: vec![t],
            workflows: vec![],
            more: false,
        }));
        assert_eq!(
            on_event(&mut app, &key(KeyCode::Enter, KeyEventKind::Press)),
            Step::Send(Request::Watch(Some(id.clone())))
        );
        assert_eq!(app.screen, Screen::Task(id));
        assert_eq!(
            on_event(&mut app, &key(KeyCode::Esc, KeyEventKind::Press)),
            Step::Send(Request::Watch(None))
        );
    }

    #[test]
    fn drain_applies_every_waiting_frame_and_reports_a_gone_worker() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new();
        assert!(drain(&mut app, &rx), "nothing waiting, worker alive");
        let snap = |n: usize| agentos_store::view::Snapshot {
            tasks: (0..n)
                .map(|i| crate::app::fixtures::task("g", i as i64, None))
                .collect(),
            workflows: vec![],
            more: false,
        };
        tx.send(crate::app::fixtures::good(snap(1))).unwrap();
        tx.send(crate::app::fixtures::good(snap(2))).unwrap();
        assert!(drain(&mut app, &rx));
        assert_eq!(app.items().len(), 2, "the last frame wins");
        drop(tx);
        assert!(!drain(&mut app, &rx));
    }
}
